"use strict";
/* Photo Vault UI. Talks only to the engine API (/api/*), so the engine can be swapped for a Rust one. */

const $ = (s, el = document) => el.querySelector(s);
const $$ = (s, el = document) => [...el.querySelectorAll(s)];
const TOKEN = $('meta[name="pv-token"]').content;
const DESKTOP = document.documentElement.classList.contains("desktop");
if (DESKTOP) {
  // An app has no page context menu, and its labels are not selected by dragging.
  document.addEventListener("contextmenu", (e) => { if (!e.target.closest("input, textarea, select, [contenteditable]")) e.preventDefault(); });
  document.addEventListener("dragstart", (e) => { if (!e.target.closest("input, textarea")) e.preventDefault(); });
}
const esc = (s) => String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
const clone = (o) => JSON.parse(JSON.stringify(o));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
function debounce(fn, ms) { let t; return (...a) => { clearTimeout(t); t = setTimeout(() => fn(...a), ms); }; }

async function api(path, opts = {}) {
  const res = await fetch(path, {
    ...opts,
    headers: { "X-PhotoVault-Token": TOKEN, "Content-Type": "application/json", ...(opts.headers || {}) },
  });
  let body = null;
  try { body = await res.json(); } catch (_) { /* empty body */ }
  if (!res.ok) { const e = new Error(typeof body?.detail === "string" ? body.detail : "Request failed"); e.status = res.status; e.detail = body?.detail; throw e; }
  return body;
}

function fmtBytes(n) {
  if (n == null) return "";
  const u = ["B", "KB", "MB", "GB", "TB"]; let i = 0, v = n;
  while (v >= 1000 && i < u.length - 1) { v /= 1000; i++; }
  return (i >= 3 ? v.toFixed(1) : Math.round(v)) + " " + u[i];
}
const fmtNum = (n) => Number(n).toLocaleString("en-US");

/* ---------- state ---------- */
const S = {
  page: "setup", cfg: null, saved: null, cfgPath: "", cfgExists: false,
  device: null, volumes: [], status: null, ollama: null, pathCheck: null, preview: null,
  errors: {}, saving: false,
  job: null, startMode: "all", backupMode: null, dismissedJob: null,
};

const PAGES = {
  setup: { label: "Setup", title: "Setup", sub: "Choose where your photos are backed up and how they are organised.", icon: '<path d="M4 7h9M17 7h3M4 17h3M11 17h9"/><circle cx="15" cy="7" r="2"/><circle cx="9" cy="17" r="2"/>' },
  backup: { label: "Backup", title: "Backup", sub: "What is on the SSD and what is still waiting on the phone.", icon: '<path d="M12 16V4M7 9l5-5 5 5"/><path d="M4 15v3a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2v-3"/>' },
  review: { label: "Review", title: "Review", sub: "Check how photos were tagged before they are filed.", icon: '<rect x="4" y="4" width="7" height="7" rx="1.5"/><rect x="13" y="4" width="7" height="7" rx="1.5"/><rect x="4" y="13" width="7" height="7" rx="1.5"/><rect x="13" y="13" width="7" height="7" rx="1.5"/>' },
  trips: { label: "Trips", title: "Trips", sub: "Photos grouped by where and when you travelled.", icon: '<path d="M12 21s-6.5-5.6-6.5-10.5a6.5 6.5 0 0 1 13 0C18.5 15.4 12 21 12 21z"/><circle cx="12" cy="10.5" r="2.3"/>', },
  health: { label: "Health", title: "Health", sub: "Prove the SSD matches the phone.", icon: '<path d="M12 3l7 3v5c0 4.5-3 8-7 10-4-2-7-5.5-7-10V6l7-3z"/><path d="M9 12l2.2 2.2L15.5 10"/>' },
};
const PRESETS = [
  { label: "Year, then year-month", layout: "{year}/{year}-{month}/{name}" },
  { label: "Year, then month number", layout: "{year}/{month}/{name}" },
  { label: "One folder per month", layout: "{year}-{month}/{name}" },
  { label: "One folder per day", layout: "{year}/{year}-{month}-{day}/{name}" },
];

/* ---------- small helpers ---------- */
function toast(msg, bad = false) {
  const el = document.createElement("div");
  el.className = "toast" + (bad ? " bad" : ""); el.textContent = msg;
  $("#toasts").append(el);
  setTimeout(() => el.remove(), bad ? 6000 : 3200);
}
const dirty = () => S.cfg && S.saved && JSON.stringify(S.cfg) !== JSON.stringify(S.saved);
const isWin = (p) => /^[A-Za-z]:/.test(p || "");
function joinPath(root, name) {
  if (!name) return root || "";
  if (/^([A-Za-z]:|\/)/.test(name)) return name;
  const sep = isWin(root) ? "\\" : "/";
  const r = (root || "").replace(/[\\/]+$/, "");
  return (r + sep + name).replace(/[\\/]+/g, sep).replace(/^([A-Za-z]:)(?=[^\\/])/, "$1\\");
}
const destPath = () => joinPath(S.cfg.ssd_root, S.cfg.backup_dir);
function sameRoot(a, b) { const n = (x) => (x || "").replace(/[\\/]+$/, "").toLowerCase(); return n(a) === n(b); }
function volumeFor(root) { return S.volumes.find((v) => sameRoot(v.path, root)); }

/* ---------- shell ---------- */
const NAV_GROUPS = { setup: "Backup", review: "Library", health: "Maintenance" };
function renderNav() {
  $("#nav").innerHTML = Object.entries(PAGES).map(([id, p]) => `${NAV_GROUPS[id] ? `<div class="nav-group">${NAV_GROUPS[id]}</div>` : ""}
    <button class="nav-item" data-nav="${id}" ${id === S.page ? 'aria-current="page"' : ""}>
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${p.icon}</svg>
      <span>${p.label}</span>${p.soon ? '<span class="soon">Soon</span>' : ""}${id === "backup" ? '<span class="live" id="navLive" hidden></span>' : ""}
    </button>`).join("");
}

function renderConn() {
  const d = S.device;
  const phone = !d ? ["", "Checking iPhone", "…"] : d.connected ? ["ok", d.name, `${d.model || "iPhone"} · iOS ${d.ios}`] : ["warn", "No iPhone", "Plug in and unlock"];
  let ssd = ["", "No drive chosen", "Open Setup"];
  if (S.cfg?.ssd_root) {
    const v = volumeFor(S.cfg.ssd_root);
    const pc = S.pathCheck, here = pc && pc.path.toLowerCase().replace(/\\/g, "/") === destPath().toLowerCase().replace(/\\/g, "/");
    const leaf = S.cfg.ssd_root.replace(/[\\/]+$/, "").split(/[\\/]/).pop() || S.cfg.ssd_root;
    if (v) ssd = ["ok", v.label || v.path, `${fmtBytes(v.free)} free`];
    else if (here && !pc.volume_missing) ssd = ["ok", leaf, pc.free != null ? `${fmtBytes(pc.free)} free` : "Folder ready"];
    else ssd = ["warn", leaf, "Not found right now"];
  }
  const row = (r) => `<div class="conn-row"><span class="dot ${r[0]}"></span><div class="t"><b>${esc(r[1])}</b><span>${esc(r[2])}</span></div></div>`;
  $("#conn").innerHTML = row(phone) + row(ssd);
}

function setHeader() {
  const p = PAGES[S.page];
  $("#title").textContent = p.title; $("#subtitle").textContent = p.sub;
  document.title = `${p.title} · Photo Vault`;
}

function route() {
  const id = (location.hash || "#setup").slice(1);
  S.page = PAGES[id] ? id : "setup";
  renderNav(); setHeader(); updateNavBadge();
  if (S.page === "backup") { if (!S.device?.connected) loadDevice(); checkPath(); }
  wide(false);
  ({ setup: renderSetup, backup: renderBackup, review: renderReview, trips: renderTrips, health: renderHealth }[S.page] || renderEmpty)();
  $("#main").scrollTo?.({ top: 0 });
}

/* ---------- Setup ---------- */
function renderSetup() {
  if (!S.cfg) { $("#page").innerHTML = '<div class="card"><div class="rows"><div class="skeleton" style="width:50%"></div></div></div>'; return; }
  const c = S.cfg;
  $("#page").innerHTML = `
  <section class="card" aria-labelledby="h-phone">
    <div class="card-head"><h2 id="h-phone">iPhone</h2><p>Photos are only ever read from the phone. Nothing on it is changed or deleted.</p></div>
    <div id="deviceCard"></div>
  </section>

  <section class="card" aria-labelledby="h-dest">
    <div class="card-head"><h2 id="h-dest">Backup location</h2><p>Pick the drive that holds your backup. The folder name below is created on it if it does not exist.</p></div>
    <div class="rows">
      <div class="row full"><div class="ctrl"><div class="vols" id="vols" role="radiogroup" aria-label="Drive"></div>
        <div class="inline"><button class="btn small" type="button" id="browse">Choose another folder…</button><span class="faint" style="font-size:13px">for a drive that is not listed</span></div>
        <div class="err" data-err="ssd_root" hidden></div></div></div>
      <div class="row"><div><div class="lab"><label for="f-backup_dir">Folder name</label></div><div class="help">Created inside the location above. You can use any name, or a full path.</div></div>
        <div class="ctrl"><input class="input" id="f-backup_dir" data-field="backup_dir" value="${esc(c.backup_dir)}" spellcheck="false" autocomplete="off">
          <div class="err" data-err="backup_dir" hidden></div>
          <div class="dest"><div class="faint" style="font-size:12.5px;margin-bottom:3px">Photos will be saved to</div><code id="destText"></code><div class="chips" id="destChips" style="margin-top:10px"></div></div></div></div>
    </div>
  </section>

  <section class="card" aria-labelledby="h-org">
    <div class="card-head"><h2 id="h-org">Folder organisation</h2><p>Files are filed by the date they were taken, read from the photo or video itself.</p></div>
    <div class="rows">
      <div class="row full"><div class="ctrl"><div class="presets" id="presets" role="radiogroup" aria-label="Folder layout"></div></div></div>
      <div class="row"><div><div class="lab"><label for="f-layout">Layout pattern</label></div><div class="help">Fields: <code>{year}</code> <code>{month}</code> <code>{day}</code> <code>{name}</code> <code>{stem}</code> <code>{ext}</code></div></div>
        <div class="ctrl"><input class="input mono" id="f-layout" data-field="layout" value="${esc(c.layout)}" spellcheck="false" autocomplete="off"><div class="err" data-err="layout" hidden></div></div></div>
      <div class="row"><div><div class="lab"><label for="f-undated_layout">Files with no date</label></div><div class="help">Where photos go when no capture date can be found.</div></div>
        <div class="ctrl"><input class="input mono" id="f-undated_layout" data-field="undated_layout" value="${esc(c.undated_layout)}" spellcheck="false" autocomplete="off"><div class="err" data-err="undated_layout" hidden></div></div></div>
      <div class="row full"><div class="ctrl"><div class="pathpreview" id="preview" aria-live="polite"></div></div></div>
    </div>
  </section>

  <section class="card" aria-labelledby="h-orig">
    <div class="card-head"><h2 id="h-orig">Originals</h2><p>Photos and videos are copied byte for byte, with all location data, edit files and Live Photo clips. Nothing is converted.</p></div>
    <div class="rows"><div class="row full"><div class="switch"><div><div class="lab" id="l-orig">Also keep the phone's original of any re-encoded video</div>
      <div class="help">If an old backup holds a shrunk or slow-motion export of a video, the original on the phone is still copied.</div></div>
      <button class="sw" role="switch" type="button" data-toggle="copy_originals_of_reencoded" aria-labelledby="l-orig" aria-checked="${c.copy_originals_of_reencoded}"></button></div></div></div>
  </section>

  <section class="card" aria-labelledby="h-trips">
    <div class="card-head"><h2 id="h-trips">Trips</h2><p>A trip is a run of days with photos taken far from home. Home is found automatically from where most of your photos are taken.</p></div>
    <div class="rows">
      <div class="row"><div><div class="lab"><label for="f-trip_radius_km">Distance that counts as away</label></div><div class="help">In kilometres from home.</div></div>
        <div class="ctrl"><input class="input" id="f-trip_radius_km" type="number" min="5" max="2000" data-field="trip_radius_km" data-type="float" value="${c.trip_radius_km}"><div class="err" data-err="trip_radius_km" hidden></div></div></div>
      <div class="row"><div><div class="lab">Home location</div><div class="help">Leave both empty to detect it. Use decimal degrees, for example 19.07 and 72.88.</div></div>
        <div class="ctrl"><div class="grid2"><label><span class="help">Latitude</span><input class="input mono" data-field="home_lat" data-type="floatnull" value="${c.home_lat ?? ""}" placeholder="Automatic" inputmode="decimal"></label>
          <label><span class="help">Longitude</span><input class="input mono" data-field="home_lon" data-type="floatnull" value="${c.home_lon ?? ""}" placeholder="Automatic" inputmode="decimal"></label></div>
          <div class="err" data-err="home_lat" hidden></div><div class="err" data-err="home_lon" hidden></div></div></div>
      <div class="row full"><div class="switch"><div><div class="lab" id="l-ft">File trip photos into trip folders</div><div class="help">Keeps each trip's photos together in their own folder on the SSD, for example Trips/2025/Udaipur (Dec 2025). Turning this on moves nothing by itself: you preview and apply it from the Trips screen, and you can always put files back.</div></div>
        <button class="sw" role="switch" type="button" data-toggle="file_trips" aria-labelledby="l-ft" aria-checked="${c.file_trips}"></button></div></div>
      <div class="row"><div><div class="lab"><label for="f-trip_layout">Trip folder pattern</label></div><div class="help">Fields: <code>{trip_dated}</code> <code>{trip}</code> <code>{trip_year}</code> <code>{year}</code> <code>{month}</code> <code>{day}</code> <code>{name}</code></div></div>
        <div class="ctrl"><input class="input mono" id="f-trip_layout" data-field="trip_layout" value="${esc(c.trip_layout)}" spellcheck="false" autocomplete="off"><div class="err" data-err="trip_layout" hidden></div><div class="help" id="tripPreview"></div></div></div>
    </div>
  </section>

  <section class="card" aria-labelledby="h-ai">
    <div class="card-head"><h2 id="h-ai">Smart tags <span class="tag" style="margin-left:8px;vertical-align:2px">Optional</span></h2><p>Use an AI model running on this computer through Ollama to suggest tags and trip names. Nothing leaves your machine, and the app works the same without it.</p></div>
    <div class="rows">
      <div class="row full"><div class="switch"><div><div class="lab" id="l-ai">Use Ollama for suggestions</div><div class="help">Rules run first. The model only looks at photos the rules are unsure about.</div></div>
        <button class="sw" role="switch" type="button" data-toggle="ai_enabled" aria-labelledby="l-ai" aria-checked="${c.ai_enabled}"></button></div></div>
      <div id="aiBody"></div>
    </div>
  </section>

  <details class="card adv"><summary>Advanced</summary>
    <div class="rows">
      <div class="row"><div><div class="lab"><label for="f-remote_roots">Phone folders to read</label></div><div class="help">Separate with commas. <code>/DCIM</code> holds your camera roll.</div></div>
        <div class="ctrl"><input class="input mono" id="f-remote_roots" data-field="remote_roots" data-type="list" value="${esc(c.remote_roots.join(", "))}" spellcheck="false"><div class="err" data-err="remote_roots" hidden></div></div></div>
      <div class="row"><div><div class="lab"><label for="f-ignore">Ignore names</label></div><div class="help">Files matching these patterns are skipped, for example <code>._*</code>.</div></div>
        <div class="ctrl"><input class="input mono" id="f-ignore" data-field="ignore" data-type="list" value="${esc(c.ignore.join(", "))}" spellcheck="false"></div></div>
      <div class="row"><div><div class="lab">Reliability</div><div class="help">If the cable drops, copying resumes where it stopped.</div></div>
        <div class="ctrl"><div class="grid2">
          <label><span class="help">Retries per file</span><input class="input" type="number" min="1" max="20" data-field="max_retries" data-type="int" value="${c.max_retries}"></label>
          <label><span class="help">Read block size (MB)</span><input class="input" type="number" min="1" max="64" data-field="chunk_mb" data-type="int" value="${c.chunk_mb}"></label></div>
          <label><span class="help">Keep this much free space on the drive (GB)</span><input class="input" type="number" min="0" max="500" step="1" data-field="free_space_margin_gb" data-type="float" value="${c.free_space_margin_gb}"></label>
          <div class="err" data-err="chunk_mb" hidden></div><div class="err" data-err="max_retries" hidden></div></div></div>
    </div>
  </details>

  <div class="savebar" id="savebar" hidden>
    <div class="msg" id="saveMsg"></div>
    <button class="btn ghost" type="button" id="discard">Discard</button>
    <button class="btn primary" type="button" id="save">Save settings</button>
  </div>`;
  renderDevice(); renderVolumes(); renderPresets(); renderAi(); refreshDest(); refreshPreview(); updateSaveBar(); showErrors();
  $$("#page .sw").forEach((b) => setSwitchLabel(b));
}

function setSwitchLabel(b) { b.title = b.getAttribute("aria-checked") === "true" ? "On" : "Off"; }

function renderDevice() {
  const el = $("#deviceCard"); if (!el) return;
  const d = S.device;
  if (!d) { el.innerHTML = '<div class="device"><span class="phone off"></span><div class="t"><h3>Looking for your iPhone</h3><div class="skeleton" style="width:55%;margin-top:8px"></div></div></div>'; return; }
  el.innerHTML = d.connected
    ? `<div class="device"><span class="phone"></span><div class="t"><h3>${esc(d.name)}</h3><p>${esc(d.model || "iPhone")} · iOS ${esc(d.ios)}</p></div><span class="pill ok"><i></i>Connected</span><button class="btn" type="button" id="recheck">Check again</button></div>`
    : `<div class="device"><span class="phone off"></span><div class="t"><h3>No iPhone connected</h3><p>${esc(d.hint || "")}</p></div><span class="pill warn"><i></i>Not found</span><button class="btn" type="button" id="recheck">Check again</button></div>`;
}

function renderVolumes() {
  const el = $("#vols"); if (!el) return;
  const cur = S.cfg.ssd_root; const match = volumeFor(cur);
  const rows = S.volumes.map((v) => {
    const used = v.total ? Math.round(((v.total - v.free) / v.total) * 100) : 0;
    const checked = match && match.path === v.path;
    return `<button class="vol" type="button" role="radio" aria-checked="${!!checked}" data-vol="${esc(v.path)}">
      <span class="radio"></span>
      <span style="min-width:0"><b>${esc(v.label)}</b> <span class="faint">${esc(v.path.replace(/\/$/, ""))}</span>
        <div class="meta num">${fmtBytes(v.free)} free of ${fmtBytes(v.total)}</div><div class="meter" aria-hidden="true"><i style="width:${used}%"></i></div></span>
      <span class="chips">${v.is_system ? '<span class="tag">System drive</span>' : ""}<span class="tag">${esc(v.fstype)}</span></span></button>`;
  });
  if (cur && !match) rows.unshift(`<button class="vol" type="button" role="radio" aria-checked="true"><span class="radio"></span><span style="min-width:0"><b>Custom location</b><div class="meta">${esc(cur)}</div></span><span class="chips"><span class="tag">Not currently listed</span></span></button>`);
  el.innerHTML = rows.join("") || '<div class="muted">No drives found.</div>';
}

function renderPresets() {
  const el = $("#presets"); if (!el) return;
  el.innerHTML = PRESETS.map((p) => `<button class="preset" type="button" role="radio" aria-checked="${S.cfg.layout === p.layout}" data-preset="${esc(p.layout)}"><b>${p.label}</b><code>${esc(p.layout)}</code></button>`).join("");
}

function renderAi() {
  const el = $("#aiBody"); if (!el) return;
  if (!S.cfg.ai_enabled) { el.innerHTML = ""; return; }
  const o = S.ollama;
  let body;
  if (!o) body = '<div class="row"><div class="muted"><span class="spin"></span> Looking for Ollama…</div></div>';
  else if (!o.reachable) body = `<div class="row"><div><div class="lab">Ollama</div></div><div class="ctrl"><span class="pill warn" style="align-self:flex-start"><i></i>Not found</span><div class="help">${esc(o.error || "")} Install Ollama and start it, then check again.</div>${aiHost()}<button class="btn small" id="aiRecheck" type="button" style="align-self:flex-start">Check again</button></div></div>`;
  else {
    const vision = o.models.filter((m) => m.vision);
    const opts = o.models.map((m) => `<option value="${esc(m.name)}" ${m.name === S.cfg.ai_model ? "selected" : ""}>${esc(m.name)} · ${m.gb} GB${m.vision ? " · can see images" : ""}</option>`).join("");
    body = `<div class="row"><div><div class="lab"><label for="f-ai_model">Model</label></div><div class="help">${vision.length ? "Models marked can see images are guessed from their names." : "None of your models can see images, so tags come from rules and file details only. A text model can still name trips."}</div></div>
      <div class="ctrl"><span class="pill ok" style="align-self:flex-start"><i></i>Ollama running · ${o.models.length} model${o.models.length === 1 ? "" : "s"}</span>
      <select class="select" id="f-ai_model" data-field="ai_model"><option value="">Choose a model…</option>${opts}</select>${aiHost()}</div></div>`;
  }
  el.innerHTML = body;
}
const aiHost = () => `<label><span class="help">Ollama address (this computer only)</span><input class="input mono" data-field="ai_host" value="${esc(S.cfg.ai_host)}" spellcheck="false"></label><div class="err" data-err="ai_host" hidden></div>`;

function refreshDest() {
  const t = $("#destText"); if (!t) return;
  t.textContent = destPath();
  const pc = S.pathCheck, chips = [];
  if (pc && pc.path.toLowerCase().replace(/\\/g, "/") === destPath().toLowerCase().replace(/\\/g, "/")) {
    if (pc.volume_missing) chips.push('<span class="pill bad"><i></i>Drive not found</span>');
    else if (!pc.writable) chips.push('<span class="pill bad"><i></i>Cannot write here</span>');
    else if (pc.exists) chips.push(`<span class="pill ok"><i></i>Folder exists · ${fmtNum(pc.file_count)} item${pc.file_count === 1 ? "" : "s"} at top level</span>`);
    else chips.push('<span class="pill ok"><i></i>Will be created</span>');
    if (pc.free != null) chips.push(`<span class="pill"><i></i>${fmtBytes(pc.free)} free</span>`);
  }
  $("#destChips").innerHTML = chips.join("");
}
const checkPath = debounce(async () => {
  if (!S.cfg?.ssd_root) return;
  try { S.pathCheck = await api("/api/path-check?path=" + encodeURIComponent(destPath())); refreshDest(); renderConn(); if (S.page === "backup" && !isActive()) renderBackup(); } catch (_) { /* leave the old result */ }
}, 250);

const refreshPreview = debounce(async () => {
  const el = $("#preview"); if (!el || !S.cfg) return;
  try {
    const r = await api(`/api/layout-preview?layout=${encodeURIComponent(S.cfg.layout)}&undated=${encodeURIComponent(S.cfg.undated_layout)}&trip_layout=${encodeURIComponent(S.cfg.trip_layout || "")}`);
    S.preview = r;
    if (r.error) el.innerHTML = '<div class="pv-row"><span class="muted">Fix the pattern above to see where files will go.</span></div>';
    else el.innerHTML = r.samples.map((s) => `<div class="pv-row"><span class="n">${esc(s.name)}${s.date ? " · " + esc(s.date) : " · no date"}</span><code>${esc(s.path)}</code></div>`).join("");
    ["layout", "undated_layout", "trip_layout"].forEach((f) => { if (r.field === f) S.errors[f] = r.error; else delete S.errors[f]; });
    const tp = $("#tripPreview"); if (tp) tp.textContent = r.trip_sample ? "Example: " + r.trip_sample : "";
    showErrors();
  } catch (_) { /* engine briefly unavailable */ }
}, 180);

function showErrors() {
  $$("[data-err]").forEach((el) => { const m = S.errors[el.dataset.err]; el.hidden = !m; el.textContent = m || ""; });
  $$("[data-field]").forEach((el) => el.classList.toggle("invalid", !!S.errors[el.dataset.field]));
}

function updateSaveBar() {
  const bar = $("#savebar"); if (!bar) return;
  const first = !S.cfgExists;
  const show = dirty() || first;
  bar.hidden = !show;
  $("#saveMsg").innerHTML = first && !dirty() ? "<b>No settings saved yet.</b> Review the options above, then save." : "<b>Unsaved changes.</b> They apply the next time you start a backup.";
  $("#discard").hidden = first && !dirty();
  $("#save").disabled = S.saving;
  $("#save").textContent = S.saving ? "Saving…" : "Save settings";
}

async function save() {
  S.saving = true; updateSaveBar(); S.errors = {};
  try {
    const r = await api("/api/config", { method: "PUT", body: JSON.stringify(S.cfg) });
    S.cfg = r.config; S.saved = clone(r.config); S.cfgExists = true; S.cfgPath = r.path;
    toast("Settings saved"); renderConn();
  } catch (e) {
    if (e.status === 422 && Array.isArray(e.detail)) {
      e.detail.forEach((d) => { const f = d.loc[d.loc.length - 1]; S.errors[f] = String(d.msg).replace(/^Value error,\s*/, ""); });
      toast("Some settings need attention", true);
    } else toast(e.message || "Could not save", true);
  } finally { S.saving = false; showErrors(); updateSaveBar(); }
}

async function loadOllama() {
  if (!S.cfg?.ai_enabled) return;
  S.ollama = null; renderAi();
  try { S.ollama = await api("/api/ollama?host=" + encodeURIComponent(S.cfg.ai_host)); }
  catch (e) { S.ollama = { reachable: false, models: [], error: "Could not check Ollama." }; }
  renderAi(); showErrors();
}
async function loadDevice() {
  S.device = null; renderDevice(); renderConn();
  try { S.device = await api("/api/device"); } catch (e) { S.device = { connected: false, hint: "The app engine did not respond." }; }
  renderDevice(); renderConn();
  if (S.page === "backup" && !isActive()) renderBackup();
  if (S.page === "health" && H.state && H.state.job.state !== "running") renderHealth(false);
}

/* ---------- Backup ---------- */
const ACTIVE = ["starting", "scanning", "copying", "paused", "cancelling"];
const isActive = () => !!S.job && ACTIVE.includes(S.job.state);
function fmtDur(s) {
  if (s == null) return "–";
  s = Math.round(s);
  const h = Math.floor(s / 3600), m = Math.floor((s % 3600) / 60), sec = s % 60;
  return h ? `${h} h ${String(m).padStart(2, "0")} min` : m ? `${m} min ${String(sec).padStart(2, "0")} s` : `${sec} s`;
}
const fmtSpeed = (b) => (b > 0 ? fmtBytes(b) + "/s" : "–");
const jobPct = (j) => (j.plan?.bytes ? Math.min(100, (j.done.bytes / j.plan.bytes) * 100) : 0);
const clock = (t) => new Date(t * 1000).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" });

function waiting() {
  const g = (k) => S.status?.counts?.[k] || { files: 0, bytes: 0 };
  return { files: g("pending").files + g("copying").files + g("failed").files, bytes: g("pending").bytes + g("copying").bytes + g("failed").bytes };
}

function preflight() {
  const out = [], w = waiting(), pc = S.pathCheck;
  const d = S.device;
  out.push(d ? (d.connected ? { ok: true, label: "iPhone connected", detail: `${d.name} · ${d.model || "iPhone"}` } : { ok: false, label: "iPhone not found", detail: d.hint || "Plug it in and unlock it." }) : { ok: null, label: "Looking for the iPhone…", detail: "" });
  out.push(!S.cfgExists ? { ok: false, label: "Settings not saved yet", detail: "Open Setup, check the options and save." } : dirty() ? { ok: false, label: "Unsaved changes in Setup", detail: "Save or discard them so the backup uses what you see." } : { ok: true, label: "Settings saved", detail: "" });
  const here = pc && pc.path.toLowerCase().replace(/\\/g, "/") === destPath().toLowerCase().replace(/\\/g, "/");
  if (!here) out.push({ ok: null, label: "Checking the backup location…", detail: "" });
  else if (pc.volume_missing) out.push({ ok: false, label: "Backup drive not found", detail: "Connect the SSD and check again." });
  else if (!pc.writable) out.push({ ok: false, label: "Cannot write to the backup location", detail: destPath() });
  else out.push({ ok: true, label: "Backup location ready", detail: destPath() });
  if (here && pc.free != null && !S.status?.has_backup) out.push({ ok: true, label: "Space", detail: `${fmtBytes(pc.free)} free. The size of your photo library is checked as copying starts.` });
  else if (here && pc.free != null) {
    const need = w.bytes + (S.cfg.free_space_margin_gb || 0) * 1e9;
    out.push(pc.free >= need ? { ok: true, label: "Enough space", detail: `${fmtBytes(pc.free)} free, ${fmtBytes(w.bytes)} to copy` } : { ok: false, label: "Not enough space", detail: `${fmtBytes(w.bytes)} to copy plus ${S.cfg.free_space_margin_gb} GB spare, but only ${fmtBytes(pc.free)} free` });
  }
  return out;
}

const STATE_PILL = {
  starting: ["", "Starting"], scanning: ["", "Reading phone"], copying: ["ok", "Copying"], paused: ["warn", "Paused"], cancelling: ["warn", "Stopping"],
  finished: ["ok", "Finished"], cancelled: ["warn", "Stopped"], error: ["bad", "Stopped by an error"],
};

function renderBackup() {
  if (!S.cfg || !S.status) { $("#page").innerHTML = '<div class="card"><div class="rows"><div class="skeleton" style="width:40%"></div></div></div>'; return; }
  S.backupMode = isActive() ? "run" : "ready";
  $("#page").innerHTML = S.backupMode === "run" ? runView() : readyView();
  if (S.backupMode === "run") paintRun();
}

function summaryCard() {
  const s = S.status;
  if (!s.has_backup) return `<section class="card"><div class="empty"><h2>No backup here yet</h2><p>Nothing has been saved to <code>${esc(destPath())}</code>. Your first backup copies everything on the phone.</p></div></section>`;
  const g = (k) => s.counts[k] || { files: 0, bytes: 0 };
  const done = g("done"), failed = g("failed"), skipped = g("skipped");
  const wait = waiting(); const wb = wait.bytes - failed.bytes;
  const total = done.bytes + wait.bytes + skipped.bytes || 1; const w = (b) => ((b / total) * 100).toFixed(2) + "%";
  return `<section class="card"><div class="stat-grid">
    <div class="stat"><div class="v num">${fmtNum(done.files)}</div><div class="l">backed up · ${fmtBytes(done.bytes)}</div></div>
    <div class="stat"><div class="v num">${fmtNum(wait.files - failed.files)}</div><div class="l">waiting on the phone · ${fmtBytes(wb)}</div></div>
    <div class="stat"><div class="v num">${fmtNum(failed.files)}</div><div class="l">failed${skipped.files ? ` · ${fmtNum(skipped.files)} skipped by choice` : ""}</div></div></div>
    <div style="padding:0 22px 20px"><div class="bar" role="img" aria-label="Share of data backed up"><i style="width:${w(done.bytes)};background:var(--ok)"></i><i style="width:${w(wb)};background:var(--line-strong)"></i><i style="width:${w(failed.bytes)};background:var(--bad)"></i></div></div></section>`;
}

function resultBanner() {
  const j = S.job; if (!j || isActive() || S.dismissedJob === j.started_at || !["finished", "cancelled", "error"].includes(j.state)) return "";
  const r = j.result || {};
  const cls = j.state === "finished" && !r.failed ? "ok" : j.state === "error" ? "bad" : "warn";
  let detail = "";
  if (j.state === "finished") detail = `${fmtNum(r.copied)} files copied (${fmtBytes(r.bytes)}) in ${fmtDur(j.elapsed)}. ${r.ssd_ok ? `All ${fmtNum(r.checked)} files on the SSD were checked and are in place.` : `The SSD check found ${r.missing} missing and ${r.wrong_size} wrong-size files.`}`;
  else if (j.state === "cancelled") detail = `${fmtNum(r.copied)} files were copied before you stopped. Run the backup again to continue.`;
  const fails = (r.failures || []).length ? `<ul class="fails">${r.failures.map((f) => `<li><b>${esc(f.name)}</b> <span class="muted">${esc(f.error)}</span></li>`).join("")}</ul>` : "";
  return `<section class="banner ${cls}"><div class="banner-body"><div class="banner-title">${esc(j.message)}</div>${detail ? `<p>${esc(detail)}</p>` : ""}${fails}</div><button class="btn ghost small" type="button" data-dismiss="${j.started_at}">Dismiss</button></section>`;
}

function readyView() {
  const checks = preflight(), blocked = checks.some((c) => c.ok === false || c.ok === null), w = waiting();
  const icon = (ok) => ok === true ? '<span class="ck ok" aria-label="OK"></span>' : ok === false ? '<span class="ck bad" aria-label="Problem"></span>' : '<span class="spin"></span>';
  const modes = [
    ["all", "Everything waiting", `${fmtNum(w.files)} files · ${fmtBytes(w.bytes)}`],
    ["test", "A small test batch first", "Copies 200 files so you can check the result before the full run"],
    ["originals", "Only originals of re-encoded videos", "Phone originals that exist as shrunk or slow-motion copies in an old backup"],
  ];
  const nothing = w.files === 0 && S.status.has_backup;
  return `${resultBanner()}${summaryCard()}
  <section class="card"><div class="card-head"><h2>Before you start</h2></div>
    <ul class="checks">${checks.map((c) => `<li>${icon(c.ok)}<div><b>${esc(c.label)}</b>${c.detail ? `<div class="muted">${esc(c.detail)}</div>` : ""}</div>${c.ok === false && /iPhone/.test(c.label) ? '<button class="btn small" type="button" id="recheck">Check again</button>' : ""}${c.ok === false && /Settings|Unsaved/.test(c.label) ? '<button class="btn small" type="button" data-nav="setup">Open Setup</button>' : ""}</li>`).join("")}</ul></section>
  <section class="card"><div class="card-head"><h2>What to copy</h2></div>
    <div class="rows"><div class="row full"><div class="ctrl"><div class="vols" role="radiogroup" aria-label="What to copy">${modes.map(([id, t, d]) => `<button class="vol" type="button" role="radio" aria-checked="${S.startMode === id}" data-mode="${id}"><span class="radio"></span><span><b>${t}</b><div class="meta">${esc(d)}</div></span><span></span></button>`).join("")}</div></div></div></div>
    <div class="startbar"><div class="muted">${nothing ? "Everything on the phone is already backed up. Run it again after taking new photos." : "The iPhone is only read, never changed. You can pause or stop at any time."}</div>
      <button class="btn primary" type="button" id="startBackup" ${blocked ? "disabled" : ""}>${S.status.has_backup ? "Start backup" : "Start first backup"}</button></div></section>`;
}

function runView() {
  return `<section class="card run"><div class="run-head"><div><span class="pill" id="jPill"><i></i><span></span></span><div class="run-msg" id="jMsg"></div></div><div class="run-btns" id="jBtns"></div></div>
    <div class="big"><div class="pct num" id="jPct"></div><div class="muted num" id="jBytes"></div></div>
    <div class="bar big-bar" role="progressbar" aria-valuemin="0" aria-valuemax="100" id="jBarWrap"><i id="jBar" style="width:0%;background:var(--accent)"></i></div>
    <div class="stat-grid run-stats">
      <div class="stat"><div class="v num" id="jFiles"></div><div class="l">files</div></div>
      <div class="stat"><div class="v num" id="jSpeed"></div><div class="l">speed</div></div>
      <div class="stat"><div class="v num" id="jEta"></div><div class="l">time left</div></div>
      <div class="stat"><div class="v num" id="jElapsed"></div><div class="l">elapsed</div></div></div></section>
  <section class="card" id="jCurCard"><div class="rows"><div class="row"><div><div class="lab">Now copying</div></div><div class="ctrl"><div id="jCur"></div><div class="bar" style="height:6px"><i id="jCurBar" style="width:0%;background:var(--ink);opacity:.7"></i></div></div></div></div></section>
  <section class="card"><div class="card-head"><h2>Activity</h2></div><ul class="log" id="jLog"></ul></section>`;
}

function paintRun() {
  const j = S.job; if (!j || !$("#jMsg")) return;
  const [cls, label] = STATE_PILL[j.state] || ["", j.state];
  const pill = $("#jPill"); pill.className = "pill " + cls; pill.lastElementChild.textContent = label;
  $("#jMsg").textContent = j.message;
  const scanning = j.state === "scanning" || j.state === "starting";
  const p = jobPct(j);
  $("#jPct").textContent = scanning ? (j.scan.seen ? `${fmtNum(j.scan.seen)} files found` : "Working…") : p.toFixed(p < 10 ? 1 : 0) + "%";
  $("#jBytes").textContent = scanning ? "" : `${fmtBytes(j.done.bytes)} of ${fmtBytes(j.plan.bytes)}`;
  $("#jBar").style.width = (scanning ? 0 : p) + "%";
  $("#jBarWrap").setAttribute("aria-valuenow", Math.round(p));
  $("#jFiles").textContent = scanning ? "–" : `${fmtNum(j.done.files + j.failed)} / ${fmtNum(j.plan.files)}`;
  $("#jSpeed").textContent = j.state === "paused" ? "Paused" : fmtSpeed(j.speed_bps);
  $("#jEta").textContent = j.state === "copying" ? fmtDur(j.eta_seconds) : "–";
  $("#jElapsed").textContent = fmtDur(j.elapsed);
  const cur = j.current;
  $("#jCurCard").hidden = !cur;
  if (cur) { $("#jCur").innerHTML = `<b>${esc(cur.name)}</b> <span class="muted num">${fmtBytes(cur.done)} of ${fmtBytes(cur.size)}</span>`; $("#jCurBar").style.width = (cur.size ? (cur.done / cur.size) * 100 : 0) + "%"; }
  const busy = j.state === "cancelling";
  const btns = $("#jBtns"), want = `${j.state === "paused" ? "resume" : "pause"}|${busy}`;
  if (btns.dataset.want !== want) {
    btns.dataset.want = want;
    btns.innerHTML = (j.state === "paused" ? '<button class="btn" type="button" id="resumeBtn">Resume</button>' : `<button class="btn" type="button" id="pauseBtn" ${busy ? "disabled" : ""}>Pause</button>`) + `<button class="btn" type="button" id="cancelBtn" ${busy ? "disabled" : ""}>Stop</button>`;
  }
  $("#jLog").innerHTML = [...j.recent].reverse().slice(0, 14).map((e) => `<li><span class="dot ${e.level === "error" ? "bad" : e.level === "warn" ? "warn" : ""}"></span><span class="faint num">${clock(e.t)}</span><span>${esc(e.text)}</span></li>`).join("") || '<li class="muted">Nothing yet.</li>';
}

function updateNavBadge() {
  const b = $("#navLive"); if (!b) return;
  if (!isActive()) { b.hidden = true; return; }
  b.hidden = false;
  b.textContent = S.job.state === "paused" ? "Paused" : S.job.state === "copying" ? Math.round(jobPct(S.job)) + "%" : "…";
}

function updateJobUI() {
  updateNavBadge();
  if (S.page !== "backup") return;
  const mode = isActive() ? "run" : "ready";
  if (S.backupMode !== mode) renderBackup();
  else if (mode === "run") paintRun();
}

async function jobCall(action, body) {
  try { S.job = await api("/api/backup/" + action, { method: "POST", body: JSON.stringify(body || {}) }); updateJobUI(); }
  catch (e) { toast(e.message || "That did not work", true); }
}
async function refreshStatus() {
  try { S.status = await api("/api/status"); } catch (_) { /* keep the old numbers */ }
  checkPath();
}
function connectEvents() {
  const es = new EventSource("/api/backup/events?token=" + encodeURIComponent(TOKEN));
  es.onmessage = (m) => {
    const prev = S.job?.state;
    S.job = JSON.parse(m.data);
    if (prev && ACTIVE.includes(prev) && !ACTIVE.includes(S.job.state)) refreshStatus().then(() => S.page === "backup" && renderBackup());
    updateJobUI();
  };
}

function renderEmpty() {
  const p = PAGES[S.page];
  const note = { health: "For now, run <code>photovault verify --deep</code> or <code>photovault sync</code> in a terminal." }[S.page];
  $("#page").innerHTML = `<div class="card empty"><h2>${p.title} is not ready yet</h2><p>${note}</p></div>`;
}

/* ---------- Review and Trips (tags and places from the SSD copy) ---------- */
const TAGS = [
  { id: "memory", label: "Memory", hint: "Recorded by your camera", color: "--t-memory" },
  { id: "trip", label: "Trip", hint: "Far from home for days", color: "--t-trip" },
  { id: "screenshot", label: "Screenshot", hint: "Screen-sized, no camera", color: "--t-shot" },
  { id: "social", label: "Chat and social", hint: "WhatsApp, saved from apps", color: "--t-social" },
  { id: "document", label: "Document", hint: "Receipts and forms", color: "--t-doc" },
  { id: "duplicate", label: "Duplicate", hint: "Same picture twice", color: "--t-dupe" },
  { id: "junk", label: "Junk", hint: "Black or accidental", color: "--t-junk" },
];
const TAG = Object.fromEntries(TAGS.map((t) => [t.id, t]));
const R = { summary: null, filter: "all", trip: null, items: [], total: 0, sel: null, loading: false, analysis: null, trips: null, groups: [], sig: "", picked: new Set(), tripForm: false, confirmDelete: null, newTrip: false, q: "" };
const A = { state: null, polling: false };
const sig = () => R.filter + "|" + (R.trip || "") + "|" + (R.q || "");
const groupedView = () => R.filter === "trip" && !R.trip;
const tripOf = (id) => (R.trips?.trips || []).find((x) => x.id === id);
const tripDates = (t) => (t.days === 1 ? fmtDay(t.start) : `${fmtDay(t.start)} to ${fmtDay(t.end)}`);
const thumb = (key, size = 320) => `/api/thumb?key=${encodeURIComponent(key)}&size=${size}&token=${encodeURIComponent(TOKEN)}`;
const glyph = (id) => `<svg aria-hidden="true"><use href="#g-${id}"/></svg>`;
const fmtDate = (t) => (t ? new Date(t * 1000).toLocaleDateString("en-GB", { day: "numeric", month: "short", year: "numeric" }) : "No date");
const fmtDay = (iso) => new Date(iso + "T00:00:00").toLocaleDateString("en-GB", { day: "numeric", month: "short", year: "numeric" });
function ago(t) { const s = Date.now() / 1000 - t; return s < 90 ? "just now" : s < 5400 ? `${Math.round(s / 60)} min ago` : s < 129600 ? `${Math.round(s / 3600)} h ago` : `${Math.round(s / 86400)} days ago`; }
const doneCount = () => (S.status?.counts?.done?.files || 0);

async function loadReviewSummary() { try { R.summary = await api("/api/review/summary"); } catch (e) { R.summary = { analyzed: false, counts: {}, total: 0 }; } }

function analyzeCard(title) {
  const a = R.analysis;
  if (a && a.state === "running") {
    const pct = a.total ? Math.round((a.done / a.total) * 100) : 0;
    return `<div class="card empty"><h2>${esc(a.message || "Working")}</h2><div class="bar" style="width:min(360px,100%);height:10px"><i style="width:${pct}%;background:var(--accent)"></i></div><p class="num">${fmtNum(a.done)} of ${fmtNum(a.total)} files</p></div>`;
  }
  const n = doneCount();
  const err = a && a.state === "error" ? `<p style="color:var(--bad)">${esc(a.error)}</p>` : "";
  return `<div class="card empty"><h2>${title}</h2><p>The app looks at each backed-up file on your SSD, on this computer only, and suggests what it is: a memory, a trip photo, a screenshot, a chat image, a duplicate or junk. Nothing is moved or deleted.</p>${err}
    <button class="btn primary" type="button" id="analyzeBtn" ${n ? "" : "disabled"}>${n ? `Analyze ${fmtNum(n)} files` : "Nothing backed up yet"}</button></div>`;
}

async function startAnalysis() {
  try { R.analysis = await api("/api/analysis/start", { method: "POST" }); } catch (e) { toast(e.message, true); return; }
  rerenderReviewPages();
  while (R.analysis && R.analysis.state === "running") {
    await sleep(600);
    try { R.analysis = await api("/api/analysis"); } catch (_) { break; }
    if (S.page === "review" || S.page === "trips") rerenderReviewPages();
  }
  if (R.analysis?.state === "error") { toast(R.analysis.error, true); rerenderReviewPages(); return; }
  await loadReviewSummary(); R.trips = null; R.items = []; R.sel = null;
  if (S.page === "review") { await loadItems(true); } else if (S.page === "trips") await loadTrips();
  rerenderReviewPages();
  toast("Analysis finished");
}
function rerenderReviewPages() { if (S.page === "review") renderReview(false); else if (S.page === "trips") renderTrips(false); }

/* Review */
async function renderReview(fresh = true) {
  wide(true);
  if (fresh) { R.analysis = R.analysis?.state === "running" ? R.analysis : null; await loadReviewSummary(); if (R.summary.analyzed) { if (!R.trips) await loadTrips(); if (R.sig !== sig()) await (groupedView() ? loadTripGroups() : loadItems(true)); } }
  if (S.page !== "review") return;
  const s = R.summary;
  if (!s) { $("#page").innerHTML = '<div class="card"><div class="rows"><div class="skeleton" style="width:40%"></div></div></div>'; return; }
  if (!s.analyzed || R.analysis?.state === "running") { $("#page").innerHTML = analyzeCard("Tag your photos"); return; }
  const c = s.counts, total = s.total || 1;
  const bar = TAGS.map((t) => `<i style="flex:${Math.max(c[t.id] || 0, 0.0001)};background:var(${t.color})"></i>`).join("");
  const drawers = [`<button class="drawer" type="button" data-filter="all" aria-pressed="${R.filter === "all" && !R.trip}"><span class="g"></span><span><b>All files</b></span><span class="n num">${fmtNum(s.total)}</span></button>`]
    .concat(TAGS.map((t) => `<button class="drawer" type="button" data-filter="${t.id}" aria-pressed="${R.filter === t.id || (t.id === "trip" && !!R.trip)}"><span class="g" style="color:var(${t.color})">${glyph(t.id)}</span><span><b>${t.label}</b><small>${t.hint}</small></span><span class="n num">${fmtNum(c[t.id] || 0)}</span></button>`)).join("");
  $("#page").innerHTML = `<div class="board">
    <aside class="rail" aria-label="Tags"><div class="bar tagbar" aria-hidden="true">${bar}</div><div id="drawers">${drawers}</div>
      <div class="rail-foot muted">${s.analysis_at ? `Analyzed ${ago(s.analysis_at)}` : ""}${s.confirmed ? ` · ${fmtNum(s.confirmed)} confirmed by you` : ""}<br><button class="btn ghost small" type="button" id="analyzeBtn">Analyze again</button></div><div class="smartbox"></div></aside>
    <section class="sheet"><div class="sheet-head"><div><button class="btn ghost small" type="button" id="tripsBack" hidden>All trips</button><h2 id="sheetTitle"></h2><p class="muted" id="sheetSub"></p></div><button class="btn small" type="button" id="confirmAll">Confirm all shown</button></div>
      ${groupedView() ? "" : `<div class="searchrow"><input class="input" id="q" type="search" placeholder="Search names and smart tags" value="${esc(R.q || "")}" aria-label="Search files" spellcheck="false"></div>`}<div class="grid" id="grid"></div><div class="more" id="more"></div></section>
    <aside class="loupe" id="loupe" aria-live="polite"></aside></div>`;
  paintGrid(); paintLoupe(); if (!A.state) loadAi(); else paintSmart();
}

async function loadItems(reset) {
  if (R.loading) return; R.loading = true;
  if (reset) { R.items = []; R.total = 0; }
  try {
    const q = new URLSearchParams({ limit: 60, offset: R.items.length });
    if (R.filter !== "all") q.set("tag", R.filter);
    if (R.trip) { q.set("trip", R.trip); q.set("order", "time"); }
    if (R.q) q.set("q", R.q);
    const r = await api("/api/review/items?" + q);
    R.items = R.items.concat(r.items); R.total = r.total; R.sig = sig();
    if (!R.items.find((i) => i.key === R.sel)) R.sel = R.items[0]?.key || null;
  } catch (e) { toast(e.message, true); }
  R.loading = false;
}

async function loadTripGroups() {
  if (R.loading) return; R.loading = true;
  try {
    if (!R.trips) await loadTrips();
    const list = (R.trips?.trips || []).slice().reverse();
    R.groups = await Promise.all(list.map((tr) => api(`/api/review/items?trip=${encodeURIComponent(tr.id)}&tag=trip&limit=12&order=time&spread=1`).then((r) => ({ trip: tr, items: r.items, total: r.total }))));
    R.groups = R.groups.filter((g) => g.total > 0);
    R.items = R.groups.flatMap((g) => g.items); R.total = R.groups.reduce((n, g) => n + g.total, 0); R.sig = sig();
    if (!R.items.find((i) => i.key === R.sel)) R.sel = R.items[0]?.key || null;
  } catch (e) { toast(e.message, true); }
  R.loading = false;
}

function tileHtml(i, hideTrip) {
  const chip = hideTrip && i.tag === "trip" ? "" : `<span class="tg" style="color:var(${TAG[i.tag].color})">${glyph(i.tag)}<span>${TAG[i.tag].label}</span></span>`;
  return `<button class="tile${i.confirmed ? " ok" : ""}${R.picked.has(i.key) ? " picked" : ""}" type="button" data-key="${esc(i.key)}" aria-pressed="${R.sel === i.key}" aria-label="${esc(i.name)}, ${TAG[i.tag].label}, ${i.conf}% sure">
    ${i.has_thumb ? `<img loading="lazy" decoding="async" alt="" src="${thumb(i.key)}">${i.kind === "video" ? '<span class="play"><svg viewBox="0 0 24 24" fill="currentColor"><path d="M8 5v14l11-7z"/></svg></span>' : ""}` : `<span class="ph">${i.kind === "video" ? '<svg viewBox="0 0 24 24" fill="currentColor"><path d="M8 5v14l11-7z"/></svg>' : ""}<small>${esc(i.name.split(".").pop().toUpperCase())}</small></span>`}
    ${chip}</button>`;
}

function paintGrid() {
  const g = $("#grid"); if (!g) return;
  $("#tripsBack").hidden = !R.trip;
  g.className = "grid";
  $("#more").innerHTML = "";
  if (groupedView()) return paintTripGroups(g);
  if (R.trip) return paintOneTrip(g);
  $("#sheetTitle").textContent = R.filter === "all" ? "All files" : TAG[R.filter].label;
  $("#sheetSub").textContent = `${fmtNum(R.total)} file${R.total === 1 ? "" : "s"}, least sure first`;
  $("#confirmAll").hidden = !R.items.length;
  g.innerHTML = R.items.map((i) => tileHtml(i, false)).join("") || '<div class="muted" style="grid-column:1/-1;padding:30px 0">Nothing with this tag.</div>';
  $("#more").innerHTML = R.items.length < R.total ? `<button class="btn" type="button" id="moreBtn">Show more (${fmtNum(R.total - R.items.length)} left)</button>` : "";
}

/* Trip photos belong together: one section per trip, a few photos from across it, then open the trip. */
function paintTripGroups(g) {
  $("#sheetTitle").textContent = "Trips";
  $("#sheetSub").textContent = `${fmtNum(R.groups.length)} trip${R.groups.length === 1 ? "" : "s"}, newest first. Open a trip to see every photo, day by day.`;
  $("#confirmAll").hidden = true;
  g.className = "groups";
  g.innerHTML = R.groups.map(({ trip: t, items, total }) => `<section class="tgroup"><header class="tg-head"><div><h3>${esc(t.name || "Unnamed trip")}</h3><p class="muted num">${tripDates(t)} · ${fmtNum(total)} file${total === 1 ? "" : "s"} · up to ${fmtNum(t.max_km)} km from home</p></div><button class="btn small" type="button" data-tripview="${esc(t.id)}">Open trip</button></header>
    <div class="grid compact">${items.map((i) => tileHtml(i, true)).join("")}</div>${total > items.length ? `<button class="more-link" type="button" data-tripview="${esc(t.id)}">+ ${fmtNum(total - items.length)} more from this trip</button>` : ""}</section>`).join("") || '<div class="muted" style="padding:30px 0">No trips yet.</div>';
}

function paintOneTrip(g) {
  const t = tripOf(R.trip);
  $("#sheetTitle").innerHTML = t ? `<input class="input name title-input" data-trip="${esc(t.id)}" value="${esc(t.name)}" placeholder="Name this trip" aria-label="Trip name" spellcheck="false">` : "Trip";
  $("#sheetSub").textContent = `${t ? tripDates(t) + " · " : ""}${fmtNum(R.total)} file${R.total === 1 ? "" : "s"}, in the order taken`;
  $("#confirmAll").hidden = !R.items.length;
  g.className = "groups";
  const days = new Map();
  R.items.forEach((i) => { const k = i.taken ? new Date(i.taken * 1000).toLocaleDateString("en-CA") : "none"; (days.get(k) || days.set(k, []).get(k)).push(i); });
  g.innerHTML = [...days].map(([k, items]) => `<section class="tgroup"><header class="tg-head day"><h3>${k === "none" ? "No date" : new Date(k + "T00:00:00").toLocaleDateString("en-GB", { weekday: "long", day: "numeric", month: "long", year: "numeric" })}</h3><span class="muted num">${fmtNum(items.length)} file${items.length === 1 ? "" : "s"}</span></header>
    <div class="grid compact">${items.map((i) => tileHtml(i, true)).join("")}</div></section>`).join("") || '<div class="muted" style="padding:30px 0">Nothing here.</div>';
  $("#more").innerHTML = R.items.length < R.total ? `<button class="btn" type="button" id="moreBtn">Show more (${fmtNum(R.total - R.items.length)} left)</button>` : "";
}

const selKeys = () => (R.picked.size ? [...R.picked] : R.sel ? [R.sel] : []);
function tripOptions() { return (R.trips?.trips || []).slice().reverse().map((tr) => `<option value="${esc(tr.id)}">${esc(tr.name || "Unnamed trip")} · ${esc(tr.start.slice(0, 7))}</option>`).join(""); }
function tripControls(items) {
  const inTrip = items.filter((i) => i.trip_id);
  const names = [...new Set(inTrip.map((i) => tripOf(i.trip_id)?.name || "a trip"))].join(", ");
  return `<div class="tripctl"><h3>Trips</h3>
    ${inTrip.length ? `<p class="muted">${inTrip.length === items.length ? "In" : `${inTrip.length} of ${items.length} are in`} ${esc(names)}.</p><button class="btn" type="button" id="tripOut">Take ${items.length === 1 ? "it" : "them"} out of the trip</button>` : ""}
    <label><span class="help">Add ${items.length === 1 ? "it" : "them"} to a trip</span><select class="select" id="tripSel"><option value="">Choose a trip…</option>${tripOptions()}<option value="__new">A new trip…</option></select></label>
    <div id="newTripBox" ${R.tripForm ? "" : "hidden"}><input class="input" id="newTripName" placeholder="Name the new trip" aria-label="New trip name" spellcheck="false"></div>
    <button class="btn" type="button" id="tripAdd">Add to trip</button></div>`;
}

function paintLoupe() {
  const el = $("#loupe"); if (!el) return;
  if (R.picked.size > 1) {
    const items = R.items.filter((i) => R.picked.has(i.key));
    el.innerHTML = `<div><div class="fname">${fmtNum(items.length)} files selected</div><p class="muted" style="font-size:13px;margin-top:2px">Ctrl or Cmd click adds one. Shift click selects a run.</p></div>
      <div class="strip sel">${items.filter((i) => i.has_thumb).slice(0, 8).map((i) => `<img alt="" src="${thumb(i.key, 160)}">`).join("")}</div>
      ${tripControls(items)}
      <div><h3>Tag them all</h3><div class="acts">${TAGS.map((x) => `<button class="btn" type="button" data-act="${x.id}" style="color:var(${x.color})">${glyph(x.id)}<span style="color:var(--ink)">${x.label}</span></button>`).join("")}</div></div>
      <button class="btn ghost" type="button" id="clearPick">Clear selection</button>`;
    return;
  }
  const it = R.items.find((i) => i.key === R.sel);
  if (!it) { el.innerHTML = '<div class="muted">Pick a file to see why it was tagged.</div>'; return; }
  const t = TAG[it.tag];
  el.innerHTML = `<div class="preview">${it.has_thumb ? `<img alt="" src="${thumb(it.key, 640)}">` : `<span class="ph big">${it.kind === "video" ? "Video" : esc(it.name.split(".").pop().toUpperCase())}<small>No preview</small></span>`}</div>
    <div><div class="fname">${esc(it.name)}</div><div class="chips" style="margin-top:8px"><span class="pill" style="color:var(${t.color})">${glyph(it.tag)}<span style="color:var(--ink)">${it.confirmed ? "Confirmed" : "Suggested"}: ${t.label}</span></span></div></div>
    ${it.confirmed ? "" : `<div class="conf"><span class="num">${it.conf}% sure</span><i><b style="width:${it.conf}%"></b></i></div>`}
    ${it.ai ? `<div><h3>Smart tags</h3><div class="chips">${it.ai.tags.map((x) => `<span class="chip">${esc(x)}</span>`).join("")}</div>${it.ai.caption ? `<p class="muted" style="font-size:13px;margin-top:6px">${esc(it.ai.caption)}</p>` : ""}${it.ai.kind && it.ai.kind !== "memory" && it.ai.kind !== it.tag ? `<p class="muted" style="font-size:13px;margin-top:4px">The model thinks this is ${it.ai.kind === "junk" ? "junk" : "a " + it.ai.kind}.</p>` : ""}</div>` : ""}
    <div><h3>Why this tag</h3><ul class="why">${it.reasons.map((r) => `<li>${esc(r)}</li>`).join("")}</ul></div>
    <dl><dt>Taken</dt><dd>${fmtDate(it.taken)}</dd>${it.trip_id ? `<dt>Trip</dt><dd>${esc(tripOf(it.trip_id)?.name || it.trip_id)}</dd>` : ""}<dt>Location</dt><dd>${it.gps ? "Saved in the file" : "None"}</dd><dt>Size</dt><dd>${fmtBytes(it.size)}</dd></dl>
    <div class="acts"><button class="btn primary wide2" type="button" data-act="confirm" ${it.confirmed && it.tag === it.suggested ? "disabled" : ""}>${glyph("check")}${it.confirmed ? "Confirmed" : "Confirm " + t.label}<kbd>Enter</kbd></button>
      ${TAGS.filter((x) => x.id !== it.tag).map((x) => `<button class="btn" type="button" data-act="${x.id}" style="color:var(${x.color})">${glyph(x.id)}<span style="color:var(--ink)">${x.label}</span></button>`).join("")}</div>
    ${tripControls([it])}
    <div class="safe muted">Tags and trips only decide how files are grouped. Nothing is moved or deleted.</div>`;
}

async function applyTag(keys, tag) {
  try {
    await api("/api/review/tag", { method: "POST", body: JSON.stringify({ keys, tag }) });
    R.items.forEach((i) => { if (keys.includes(i.key)) { i.confirmed = true; if (tag) i.tag = tag; } });
    await loadReviewSummary();
    if (groupedView() && tag && tag !== "trip") { R.groups.forEach((g) => { const n = g.items.length; g.items = g.items.filter((i) => !keys.includes(i.key)); g.total -= n - g.items.length; }); R.items = R.groups.flatMap((g) => g.items); if (!R.items.find((i) => i.key === R.sel)) R.sel = R.items[0]?.key || null; }
    else if (R.filter !== "all" && !R.trip && tag && tag !== R.filter) { R.items = R.items.filter((i) => !keys.includes(i.key)); R.total -= keys.length; if (!R.items.find((i) => i.key === R.sel)) R.sel = R.items[0]?.key || null; }
    renderReview(false);
  } catch (e) { toast(e.message, true); }
}

/* Filing trips into folders on the SSD */
const O = { preview: null, state: null, polling: false };
async function loadFiling() {
  try { [O.preview, O.state] = await Promise.all([api("/api/organize/preview"), api("/api/organize")]); } catch (_) { return; }
  paintFiling(); if (O.state.state === "running") pollFiling();
}
function paintFiling() {
  const el = $(".filing"); if (!el) return;
  const p = O.preview, st = O.state, saved = !dirty();
  if (st && st.state === "running") { const pc = st.total ? Math.round((st.done / st.total) * 100) : 0; el.innerHTML = `<section class="card"><div class="card-head"><h2>Filing trips into folders</h2><p>${esc(st.message)}</p></div><div style="padding:6px 22px 20px"><div class="bar" style="height:10px"><i style="width:${pc}%;background:var(--accent)"></i></div><div class="startbar" style="padding:12px 0 0;border:0"><span class="muted num">${fmtNum(st.done)} of ${fmtNum(st.total)} files</span><button class="btn" type="button" id="orgStop">Stop</button></div></div></section>`; return; }
  const err = st && st.state === "error" ? `<p style="color:var(--bad);padding:0 22px">${esc(st.error)}</p>` : "";
  const res = st && st.result && (st.state === "done" || st.state === "cancelled") ? `<p class="muted" style="padding:0 22px 6px">Last run: ${fmtNum(st.result.moved)} files moved${st.result.failed_count ? `, ${fmtNum(st.result.failed_count)} could not be moved` : ""}${st.result.stopped ? ", stopped early" : ""}. ${st.result.ssd_ok ? "Every file on the SSD is still in place." : "The SSD check found a problem. Open Health."}</p>` : "";
  if (!p || (!p.enabled && p.total === 0)) { el.innerHTML = `<section class="card"><div class="card-head"><h2>Filing trips into folders</h2><p>Off. Turn it on in Setup to keep each trip's photos together in one folder on the SSD, such as Trips/2025/Udaipur (Dec 2025).</p></div>${err}<div class="startbar"><span class="muted">Nothing on the SSD is moved until you apply it here.</span><button class="btn" type="button" data-nav="setup">Open Setup</button></div></section>`; return; }
  const rows = p.samples.map((s) => `<li><span class="muted">${esc(s.name)}</span><code>${esc(s.from)}</code><span class="arrow">to</span><code>${esc(s.to)}</code></li>`).join("");
  const headline = p.total === 0 ? "Every trip photo is already in its trip folder." : `${p.into ? `${fmtNum(p.into)} file${p.into === 1 ? "" : "s"} will move into trip folders` : ""}${p.into && p.back ? " and " : ""}${p.back ? `${fmtNum(p.back)} will go back to their dated folders` : ""}.`;
  el.innerHTML = `<section class="card"><div class="card-head"><h2>Filing trips into folders</h2><p>${p.enabled ? "On." : "Off, so photos filed earlier will go back."} Files are renamed inside the SSD, never copied or deleted, and they can all be put back.</p></div>${err}${res}
    <div class="plan"><b>${headline}</b>${rows ? `<ul>${rows}</ul>${p.total > p.samples.length ? `<p class="muted">and ${fmtNum(p.total - p.samples.length)} more</p>` : ""}` : ""}</div>
    <div class="startbar"><span class="muted">${saved ? "Uses the settings you saved." : "You have unsaved changes in Setup. Save them first."}</span><button class="btn primary" type="button" id="orgApply" ${p.total && saved ? "" : "disabled"}>${p.total ? "Apply" : "Nothing to do"}</button></div></section>`;
}
async function pollFiling() {
  if (O.polling) return; O.polling = true;
  try { while (true) { await sleep(700); try { O.state = await api("/api/organize"); } catch (_) { break; } paintFiling(); if (O.state.state !== "running") break; } } finally { O.polling = false; }
  if (O.state?.state === "done") toast("Trip folders updated");
  R.sig = ""; await loadFiling();
}
document.addEventListener("click", async (ev) => {
  const t = ev.target.closest("button"); if (!t) return;
  if (t.id === "orgApply") { try { O.state = await api("/api/organize/start", { method: "POST" }); paintFiling(); pollFiling(); } catch (e) { toast(e.message, true); } return; }
  if (t.id === "orgStop") { try { await api("/api/organize/cancel", { method: "POST" }); } catch (e) { toast(e.message, true); } }
});

/* Smart tags (optional, a local Ollama) */
async function loadAi() { try { A.state = await api("/api/ai"); } catch (e) { A.state = { job: { state: "idle" }, ready: false, done: { photos: 0, trips: 0 } }; } paintSmart(); if (A.state.job.state === "running") pollAi(); }
function smartHtml() {
  const a = A.state; if (!a) return "";
  const j = a.job, running = j.state === "running";
  if (!a.ready) return `<div class="smart off"><b>Smart tags</b><p class="muted">Off. Turn them on in Setup to get trip titles and picture descriptions from a model on this computer.</p><button class="btn ghost small" type="button" data-nav="setup">Open Setup</button></div>`;
  if (running) return `<div class="smart"><b>Smart tags</b><p class="muted">${esc(j.kind === "trips" ? "Naming trips" : "Describing pictures")}, ${fmtNum(j.done)} of ${fmtNum(j.total)}</p><div class="bar" style="height:6px"><i style="width:${j.total ? (j.done / j.total) * 100 : 0}%;background:var(--accent)"></i></div><button class="btn ghost small" type="button" id="aiStop">Stop</button></div>`;
  const err = j.state === "error" ? `<p style="color:var(--bad);font-size:12.5px">${esc(j.error)}</p>` : "";
  const photos = S.page === "review";
  return `<div class="smart"><b>Smart tags</b><p class="muted">${esc(a.model)} · ${fmtNum(a.done.photos)} pictures described, ${fmtNum(a.done.trips)} trips named</p>${err}
    ${photos ? '<button class="btn small" type="button" id="aiPhotos">Describe uncertain photos</button>' : '<button class="btn small" type="button" id="aiTrips">Suggest trip names</button>'}</div>`;
}
function paintSmart() { $$(".smartbox").forEach((el) => { el.innerHTML = smartHtml(); }); }
async function pollAi() {
  if (A.polling) return; A.polling = true; let kind = null;
  try { while (true) { await sleep(800); await loadAiQuiet(); kind = A.state.job.kind; if (A.state.job.state !== "running") break; paintSmart(); } } finally { A.polling = false; }
  paintSmart();
  if (A.state.job.state === "done") { toast(kind === "trips" ? "Trip names suggested" : "Pictures described"); R.sig = ""; R.trips = null; if (S.page === "review") { await loadTrips(); await (groupedView() ? loadTripGroups() : loadItems(true)); renderReview(false); } else if (S.page === "trips") { await loadTrips(); renderTrips(false); } }
  else if (A.state.job.state === "error") toast(A.state.job.error, true);
}
async function loadAiQuiet() { try { A.state = await api("/api/ai"); } catch (_) { /* keep the last state */ } }
async function aiStart(kind) {
  try { await api("/api/ai/start", { method: "POST", body: JSON.stringify({ kind, limit: 300 }) }); await loadAiQuiet(); paintSmart(); pollAi(); }
  catch (e) { toast(e.message, true); }
}
document.addEventListener("click", async (ev) => {
  const t = ev.target.closest("button"); if (!t) return;
  if (t.id === "aiPhotos") return aiStart("photos");
  if (t.id === "aiTrips") return aiStart("trips");
  if (t.id === "aiStop") { try { await api("/api/ai/cancel", { method: "POST" }); } catch (e) { toast(e.message, true); } return; }
  if (t.dataset.useai) { try { await api("/api/trips/" + encodeURIComponent(t.dataset.useai), { method: "PUT", body: JSON.stringify({ name: t.dataset.title }) }); toast("Trip renamed"); await loadTrips(); renderTrips(false); } catch (e) { toast(e.message, true); } }
});
document.addEventListener("input", debounce((ev) => {
  if (ev.target.id !== "q" || S.page !== "review") return;
  R.q = ev.target.value.trim(); R.sel = null; R.picked.clear();
  loadItems(true).then(() => { paintGrid(); paintLoupe(); });
}, 300));

/* Changing trips: add, take out, create, dissolve */
async function afterTripChange() {
  R.trips = null; await loadTrips(); await loadReviewSummary();
  if (R.trip && !tripOf(R.trip)) { R.trip = null; R.filter = "trip"; }
  R.sig = "";
  if (S.page === "review") { await (groupedView() ? loadTripGroups() : loadItems(true)); renderReview(false); }
  else if (S.page === "trips") renderTrips(false);
}
async function tripChange(path, method, body, message) {
  try { await api(path, { method, ...(body ? { body: JSON.stringify(body) } : {}) }); toast(message); R.picked.clear(); await afterTripChange(); }
  catch (e) { toast(e.message, true); }
}
async function tripAddClick() {
  const keys = selKeys(); if (!keys.length) return;
  const sel = $("#tripSel")?.value;
  if (!sel) return toast("Choose a trip first", true);
  if (sel === "__new") {
    const name = ($("#newTripName")?.value || "").trim(); if (!name) return toast("Give the new trip a name", true);
    const dates = R.items.filter((i) => keys.includes(i.key) && i.taken).map((i) => i.taken).sort((a, b) => a - b);
    const day = (ts) => new Date(ts * 1000).toLocaleDateString("en-CA"), today = new Date().toLocaleDateString("en-CA");
    try { const r = await api("/api/trips", { method: "POST", body: JSON.stringify({ name, start: dates.length ? day(dates[0]) : today, end: dates.length ? day(dates[dates.length - 1]) : today, keys }) }); toast(`Trip created with ${fmtNum(r.files)} files`); R.picked.clear(); R.tripForm = false; await afterTripChange(); }
    catch (e) { toast(e.message, true); }
    return;
  }
  return tripChange(`/api/trips/${encodeURIComponent(sel)}/items`, "POST", { keys }, `Added to ${tripOf(sel)?.name || "the trip"}`);
}
async function createTripClick() {
  const name = ($("#ntName")?.value || "").trim(), start = $("#ntStart")?.value, end = $("#ntEnd")?.value;
  if (!name || !start || !end) return toast("Fill in a name and both dates", true);
  try { const r = await api("/api/trips", { method: "POST", body: JSON.stringify({ name, start, end }) }); toast(`Trip created with ${fmtNum(r.files)} files from those days`); R.newTrip = false; await afterTripChange(); }
  catch (e) { toast(e.message, true); }
}

/* Trips */
async function loadTrips() { try { R.trips = await api("/api/trips"); } catch (e) { R.trips = { trips: [], home: null, radius_km: 60 }; } }

async function renderTrips(fresh = true) {
  wide(true);
  if (fresh) { R.analysis = R.analysis?.state === "running" ? R.analysis : null; await loadReviewSummary(); await loadTrips(); }
  if (S.page !== "trips") return;
  const s = R.summary;
  if (!s) return;
  if (!s.analyzed || R.analysis?.state === "running") { $("#page").innerHTML = analyzeCard("Find your trips"); return; }
  const T = R.trips, list = T?.trips || [];
  const newForm = `<div class="newtrip">${R.newTrip ? `<div class="nt-form"><label><span class="help">Name</span><input class="input" id="ntName" placeholder="For example, Train to Pune" spellcheck="false"></label><label><span class="help">First day</span><input class="input" id="ntStart" type="date"></label><label><span class="help">Last day</span><input class="input" id="ntEnd" type="date"></label><button class="btn primary" type="button" id="createTripBtn">Create trip</button><button class="btn ghost" type="button" id="newTripBtn">Cancel</button></div><p class="muted" style="font-size:13px;margin-top:8px">Every photo and video taken on those days joins the trip. Take any out afterwards from the Review screen.</p>` : `<button class="btn" type="button" id="newTripBtn">New trip</button>`}</div>`;
  const home = T?.home ? `${T.home[0].toFixed(2)}, ${T.home[1].toFixed(2)}` : null;
  const info = `<p class="muted">${home ? `Home is where most of your photos were taken (${home}). ` : ""}A trip is a run of days with photos more than ${T?.radius_km || 60} km away. Change this in Setup.</p>`;
  if (!list.length) { $("#page").innerHTML = `${newForm}<div class="card empty"><h2>No trips found yet</h2><p>Trips come from the location saved in your photos. They will show up as more of your library is backed up.</p><button class="btn" type="button" id="analyzeBtn">Analyze again</button></div>${info}`; return; }
  const a0 = Date.parse(list[0].start) - 20 * 864e5, a1 = Date.parse(list[list.length - 1].end) + 20 * 864e5, span = a1 - a0 || 1;
  const pos = (iso) => ((Date.parse(iso) - a0) / span) * 100;
  let months = ""; const d = new Date(a0); d.setDate(1); d.setMonth(d.getMonth() + 1);
  const every = Math.max(1, Math.round(span / 864e5 / 30 / 8));
  for (let i = 0; d.getTime() < a1; d.setMonth(d.getMonth() + 1), i++) if (i % every === 0) months += `<div class="m" style="left:${((d - a0) / span) * 100}%">${d.toLocaleDateString("en-GB", { month: "short", year: "2-digit" })}</div>`;
  const segs = list.map((t) => `<div class="seg${t.name ? "" : " ghost"}" style="left:${pos(t.start)}%;width:max(${pos(t.end) - pos(t.start)}%,10px)" title="${esc(t.name || "Unnamed trip")}"></div>`).join("");
  $("#page").innerHTML = `${info}<div class="smartbox"></div><div class="filing"></div>${newForm}<div class="axis" role="img" aria-label="Timeline of trips">${months}${segs}</div><div class="trips">${list.slice().reverse().map(tripCard).join("")}</div>`;
  if (!A.state) loadAi(); else paintSmart();
  loadFiling();
}
function tripCard(t) {
  const strip = t.sample.map((k) => `<img loading="lazy" decoding="async" alt="" src="${thumb(k, 160)}">`).join("");
  const days = t.days === 1 ? fmtDay(t.start) : `${fmtDay(t.start)} to ${fmtDay(t.end)}`;
  return `<article class="trip"><div class="trip-main"><div class="trip-name"><input class="input name" data-trip="${esc(t.id)}" value="${esc(t.name)}" placeholder="Name this trip" aria-label="Trip name" spellcheck="false"></div>
    <p class="muted num">${days} · ${t.days} day${t.days === 1 ? "" : "s"}</p><p class="muted num">${fmtNum(t.items)} files · up to ${fmtNum(t.max_km)} km from home</p>
    <div class="chips">${t.places.map((p) => `<span class="chip">${esc(p)}</span>`).join("")}</div>${t.ai && t.ai.title !== t.name ? `<div class="aisug"><span class="pill"><i></i>Suggested</span> <b>${esc(t.ai.title)}</b> <button class="btn small" type="button" data-useai="${esc(t.id)}" data-title="${esc(t.ai.title)}">Use this name</button>${t.ai.summary ? `<p class="muted">${esc(t.ai.summary)}</p>` : ""}</div>` : ""}</div>
    <div class="strip">${strip}</div><div class="trip-actions"><button class="btn" type="button" data-tripview="${esc(t.id)}">Review files</button>${R.confirmDelete === t.id ? `<span class="muted" style="font-size:12.5px">Photos stay on the SSD.</span><button class="btn danger small" type="button" data-del-trip="${esc(t.id)}">Yes, dissolve it</button><button class="btn ghost small" type="button" data-del-cancel="1">Keep</button>` : `<button class="btn ghost small" type="button" data-del-trip="${esc(t.id)}">Delete trip</button>`}</div></article>`;
}

function wide(on) { $("#page").classList.toggle("wide", on); $(".top").classList.toggle("wide", on); }

/* events for these pages */
document.addEventListener("click", (ev) => {
  const t = ev.target.closest("button"); if (!t) return;
  if (t.id === "analyzeBtn") return startAnalysis();
  if (t.dataset.filter) { R.filter = t.dataset.filter; R.trip = null; R.sel = null; return (groupedView() ? loadTripGroups() : loadItems(true)).then(() => renderReview(false)); }
  if (t.id === "tripsBack") { R.trip = null; R.filter = "trip"; R.sel = null; return loadTripGroups().then(() => renderReview(false)); }
  if (t.dataset.key) {
    const k = t.dataset.key;
    if (ev.shiftKey && R.sel) {
      const a = R.items.findIndex((i) => i.key === R.sel), b = R.items.findIndex((i) => i.key === k);
      R.items.slice(Math.min(a, b), Math.max(a, b) + 1).forEach((i) => R.picked.add(i.key));
    } else if (ev.ctrlKey || ev.metaKey) { if (!R.picked.size && R.sel) R.picked.add(R.sel); R.picked.has(k) ? R.picked.delete(k) : R.picked.add(k); R.sel = k; }
    else { R.picked.clear(); R.sel = k; }
    paintGrid(); paintLoupe(); return;
  }
  if (t.id === "clearPick") { R.picked.clear(); paintGrid(); paintLoupe(); return; }
  if (t.id === "tripAdd") return tripAddClick();
  if (t.id === "tripOut") return tripChange(`/api/trips/${encodeURIComponent(R.items.find((i) => selKeys().includes(i.key) && i.trip_id)?.trip_id)}/remove`, "POST", { keys: selKeys() }, "Taken out of the trip");
  if (t.id === "newTripBtn") { R.newTrip = !R.newTrip; return renderTrips(false); }
  if (t.id === "createTripBtn") return createTripClick();
  if (t.dataset.delTrip) { if (R.confirmDelete === t.dataset.delTrip) { const id = R.confirmDelete; R.confirmDelete = null; return tripChange("/api/trips/" + encodeURIComponent(id), "DELETE", null, "Trip dissolved. The photos are still on your SSD."); } R.confirmDelete = t.dataset.delTrip; return renderTrips(false); }
  if (t.dataset.delCancel) { R.confirmDelete = null; return renderTrips(false); }
  if (t.id === "moreBtn") return loadItems(false).then(() => paintGrid());
  if (t.id === "confirmAll") return applyTag(R.items.map((i) => i.key), null);
  if (t.dataset.act) { const keys = selKeys(); if (!keys.length) return; return applyTag(keys, t.dataset.act === "confirm" ? null : t.dataset.act); }
  if (t.dataset.tripview) { R.trip = t.dataset.tripview; R.filter = "all"; R.sel = null; if (S.page === "review") return loadItems(true).then(() => { R.sig = sig(); renderReview(false); $("#main").scrollTo?.({ top: 0 }); }); location.hash = "#review"; }
});
document.addEventListener("change", async (ev) => {
  if (ev.target.id === "tripSel") { R.tripForm = ev.target.value === "__new"; const b = $("#newTripBox"); if (b) { b.hidden = !R.tripForm; if (R.tripForm) $("#newTripName")?.focus(); } return; }
  const el = ev.target.closest("[data-trip]"); if (!el) return;
  try { await api("/api/trips/" + encodeURIComponent(el.dataset.trip), { method: "PUT", body: JSON.stringify({ name: el.value }) }); toast("Trip name saved"); await loadTrips(); if (S.page === "review") paintLoupe(); }
  catch (e) { toast(e.message, true); }
});
document.addEventListener("keydown", (ev) => {
  if (S.page !== "review" || !R.items.length || /INPUT|TEXTAREA|SELECT/.test(document.activeElement?.tagName || "")) return;
  const i = R.items.findIndex((x) => x.key === R.sel);
  if (ev.key === "ArrowRight" && R.items[i + 1]) R.sel = R.items[i + 1].key;
  else if (ev.key === "ArrowLeft" && R.items[i - 1]) R.sel = R.items[i - 1].key;
  else if (ev.key === "Enter" && R.sel && document.activeElement?.tagName !== "BUTTON") return applyTag([R.sel], null);
  else if (/^[1-7]$/.test(ev.key) && R.sel) return applyTag([R.sel], TAGS[Number(ev.key) - 1].id);
  else return;
  ev.preventDefault(); paintGrid(); paintLoupe();
  $(`.tile[aria-pressed="true"]`)?.scrollIntoView({ block: "nearest" });
});

/* ---------- Health ---------- */
const H = { state: null, polling: false };
async function loadHealth() { try { H.state = await api("/api/health/state"); } catch (e) { H.state = { job: { state: "idle" }, last: { sync: null, deep: null } }; } }

async function renderHealth(fresh = true) {
  wide(false);
  if (fresh || !H.state) await loadHealth();
  if (S.page !== "health") return;
  if (!S.status?.has_backup) { $("#page").innerHTML = '<div class="card empty"><h2>Nothing to check yet</h2><p>Health compares your iPhone with the SSD. It needs a backup first.</p><button class="btn primary" type="button" data-nav="backup">Open Backup</button></div>'; return; }
  const job = H.state.job, last = H.state.last, running = job.state === "running";
  const sync = last.sync, deep = last.deep;
  const problems = [sync, deep].filter(Boolean).reduce((n, r) => n + r.missing.count + r.wrong_size.count + r.corrupt.count, 0);
  const waiting = sync ? sync.waiting : 0;
  let banner;
  if (!sync && !deep) banner = ["", "Not checked yet", "Run a check below to see whether the SSD matches your iPhone."];
  else if (problems) banner = ["bad", `${fmtNum(problems)} file${problems === 1 ? "" : "s"} need attention`, "Files that were backed up are missing or damaged on the SSD. Details are below."];
  else if (waiting) banner = ["warn", `${fmtNum(waiting)} file${waiting === 1 ? "" : "s"} on the iPhone not backed up yet`, "Everything already on the SSD is fine. Run a backup to copy the rest."];
  else banner = ["ok", "Everything matches", "The SSD has every file from the iPhone, and the files that were checked are intact."];
  const when = [sync && `Phone comparison ${ago(sync.at)}`, deep && `File check ${ago(deep.at)}`].filter(Boolean).join(" · ");
  const busyBackup = isActive();
  const phoneOk = S.device?.connected;
  const dis = running || busyBackup ? "disabled" : "";

  const runCard = running ? `<section class="card run"><div class="run-head"><div><span class="pill ok"><i></i>Running</span><div class="run-msg">${esc(job.message)}</div></div><div class="run-btns"><button class="btn" type="button" id="stopCheck">Stop</button></div></div>
    <div class="big"><div class="pct num">${job.total ? Math.round((job.done / job.total) * 100) + "%" : fmtNum(job.done)}</div><div class="muted num">${job.total ? `${fmtNum(job.done)} of ${fmtNum(job.total)} files` : "files read so far"}</div></div>
    <div class="bar big-bar"><i style="width:${job.total ? (job.done / job.total) * 100 : 0}%;background:var(--accent)"></i></div></section>` : "";
  const stats = (pairs) => `<div class="mini">${pairs.map(([v, l]) => `<div><b class="num">${fmtNum(v)}</b><span>${l}</span></div>`).join("")}</div>`;
  const syncStats = sync ? stats([[sync.on_phone, "on the iPhone"], [sync.backed_up, "backed up"], [sync.waiting, "waiting"], [sync.only_on_ssd, "only on the SSD"]]) : "";
  const deepStats = deep ? stats([[deep.checked, "files re-read"], [deep.missing.count + deep.wrong_size.count + deep.corrupt.count, "problems"]]) : "";
  const err = job.state === "error" ? `<div class="banner bad"><div class="banner-body"><div class="banner-title">The check could not finish</div><p>${esc(job.error || "")}</p></div></div>` : "";

  $("#page").innerHTML = `${err}<section class="banner ${banner[0]}"><div class="banner-body"><div class="banner-title">${esc(banner[1])}</div><p>${esc(banner[2])}${when ? ` <span class="faint">${esc(when)}</span>` : ""}</p></div></section>${runCard}
  <section class="card"><div class="card-head"><h2>Compare the iPhone with the SSD</h2><p>Lists what is on the phone right now and checks that each file is in the backup. Fast, and nothing is changed.</p></div>
    ${syncStats}<div class="startbar"><div class="muted">${phoneOk ? "iPhone connected." : "Connect and unlock the iPhone first."}</div><button class="btn primary" type="button" id="runSync" ${dis} ${phoneOk ? "" : "disabled"}>Compare now</button></div></section>
  <section class="card"><div class="card-head"><h2>Verify every file on the SSD</h2><p>Reads each backed-up file again and compares it with the fingerprint saved when it was copied. This is the only way to catch silent damage, and it takes a while on a big library.</p></div>
    ${deepStats}<div class="startbar"><div class="muted">${busyBackup ? "Wait for the running backup to finish." : "You can stop it at any time."}</div><button class="btn" type="button" id="runDeep" ${dis}>Verify files</button></div></section>
  ${problemCard(sync, deep)}`;
  if (running) pollHealth();
}

function fileList(title, r) {
  if (!r.count) return "";
  return `<details class="plist"><summary><b>${title}</b> <span class="muted num">${fmtNum(r.count)}</span></summary><ul>${r.shown.map((n) => `<li><code>${esc(n)}</code></li>`).join("")}${r.count > r.shown.length ? `<li class="muted">and ${fmtNum(r.count - r.shown.length)} more</li>` : ""}</ul></details>`;
}
function problemCard(sync, deep) {
  const rs = [sync, deep].filter(Boolean); const sum = (k) => rs.reduce((n, r) => n + r[k].count, 0);
  if (!(sum("missing") + sum("wrong_size") + sum("corrupt"))) return "";
  const merged = (k) => ({ count: sum(k), shown: rs.flatMap((r) => r[k].shown).slice(0, 40) });
  return `<section class="card"><div class="card-head"><h2>What needs fixing</h2><p>Fixing queues these files to be copied again from the iPhone the next time you back up. A damaged copy is moved aside into a quarantine folder, never deleted. Files that came from an old backup folder cannot be fetched again and are only reported.</p></div>
    <div class="rows">${fileList("Missing from the SSD", merged("missing"))}${fileList("Wrong size", merged("wrong_size"))}${fileList("Damaged (fingerprint differs)", merged("corrupt"))}</div>
    <div class="startbar"><div class="muted">Your iPhone must still hold these files.</div><button class="btn primary" type="button" id="repairBtn" ${isActive() ? "disabled" : ""}>Queue for the next backup</button></div></section>`;
}

async function pollHealth() {
  if (H.polling) return; H.polling = true;
  try {
    while (S.page === "health") {
      await sleep(700); await loadHealth();
      if (H.state.job.state !== "running") break;
      renderHealth(false);
    }
  } finally { H.polling = false; }
  if (S.page === "health") { renderHealth(false); if (H.state.job.state === "done") toast("Check finished"); }
}
async function startCheck(kind) {
  try { await api("/api/health/start", { method: "POST", body: JSON.stringify({ kind }) }); await loadHealth(); renderHealth(false); }
  catch (e) { toast(e.message, true); }
}

document.addEventListener("click", async (ev) => {
  const t = ev.target.closest("button"); if (!t) return;
  if (t.id === "runSync") return startCheck("sync");
  if (t.id === "runDeep") return startCheck("deep");
  if (t.id === "stopCheck") { try { await api("/api/health/cancel", { method: "POST" }); } catch (e) { toast(e.message, true); } return; }
  if (t.id === "repairBtn") {
    try { const r = await api("/api/health/repair", { method: "POST" }); await refreshStatus(); toast(`${fmtNum(r.queued)} file${r.queued === 1 ? "" : "s"} queued for the next backup${r.legacy_skipped ? `; ${r.legacy_skipped} from an old folder cannot be re-copied` : ""}`); await renderHealth(true); }
    catch (e) { toast(e.message, true); }
  }
});

/* ---------- folder dialog ---------- */
const FD = { path: "", parent: null, onPick: null };
async function fdLoad(path) {
  try {
    const r = await api("/api/fs?path=" + encodeURIComponent(path || ""));
    FD.path = r.path; FD.parent = r.parent;
    $("#fdCurrent").textContent = r.path || "This computer";
    $("#fdUp").disabled = r.parent == null;
    $("#fdPick").disabled = !r.path;
    $("#fdHint").textContent = r.path ? "" : "Pick a drive first";
    $("#fdList").innerHTML = r.entries.length ? r.entries.map((e) => `<li><button type="button" data-fd="${esc(e.path)}"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linejoin="round"><path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/></svg>${esc(e.name)}</button></li>`).join("") : '<li class="none">No folders in here.</li>';
  } catch (e) { toast(e.message || "Could not open that folder", true); }
}
async function openFolderDialog(start, onPick) {
  // In the desktop app the operating system's own folder picker is used; in a browser, the built-in one.
  const native = window.__TAURI__?.dialog;
  if (native?.open) {
    try {
      const p = await native.open({ directory: true, multiple: false, title: "Choose a folder", defaultPath: start || undefined });
      if (p) onPick(String(p).replace(/\\/g, "/"));
      return;
    } catch (_) { /* fall back to the built-in picker */ }
  }
  FD.onPick = onPick; $("#folderDialog").showModal(); fdLoad(start || "");
}

/* ---------- events ---------- */
document.addEventListener("click", async (ev) => {
  const t = ev.target.closest("button, .sw"); if (!t) return;
  if (t.dataset.nav) { location.hash = "#" + t.dataset.nav; return; }
  if (t.dataset.vol) { S.cfg.ssd_root = t.dataset.vol; renderVolumes(); refreshDest(); checkPath(); updateSaveBar(); renderConn(); return; }
  if (t.dataset.preset) { S.cfg.layout = t.dataset.preset; $("#f-layout").value = S.cfg.layout; renderPresets(); refreshPreview(); updateSaveBar(); return; }
  if (t.dataset.toggle) {
    const f = t.dataset.toggle, on = t.getAttribute("aria-checked") !== "true";
    S.cfg[f] = on; t.setAttribute("aria-checked", on); setSwitchLabel(t);
    if (f === "ai_enabled") { renderAi(); if (on) loadOllama(); }
    updateSaveBar(); return;
  }
  if (t.id === "recheck") return loadDevice();
  if (t.id === "aiRecheck") return loadOllama();
  if (t.id === "browse") return openFolderDialog(S.cfg.ssd_root, (p) => { S.cfg.ssd_root = p; renderVolumes(); refreshDest(); checkPath(); updateSaveBar(); renderConn(); });
  if (t.id === "save") return save();
  if (t.id === "discard") { S.cfg = clone(S.saved); S.errors = {}; renderSetup(); checkPath(); return; }
  if (t.dataset.mode) { S.startMode = t.dataset.mode; return renderBackup(); }
  if (t.dataset.dismiss) { S.dismissedJob = Number(t.dataset.dismiss); return renderBackup(); }
  if (t.id === "startBackup") { const m = S.startMode; return jobCall("start", { limit: m === "test" ? 200 : null, only_reencoded_originals: m === "originals" }); }
  if (t.id === "pauseBtn") return jobCall("pause");
  if (t.id === "resumeBtn") return jobCall("resume");
  if (t.id === "cancelBtn") return jobCall("cancel");
  if (t.dataset.fd !== undefined) return fdLoad(t.dataset.fd);
  if (t.id === "fdUp") return fdLoad(FD.parent || "");
  if (t.id === "fdPick") { const p = FD.path; $("#folderDialog").close(); FD.onPick?.(p); return; }
});

document.addEventListener("input", (ev) => {
  const el = ev.target.closest("[data-field]"); if (!el || !S.cfg) return;
  const f = el.dataset.field, type = el.dataset.type;
  let v = el.value;
  if (type === "list") v = v.split(",").map((x) => x.trim()).filter(Boolean);
  else if (type === "int") v = parseInt(v, 10) || 0;
  else if (type === "float") v = parseFloat(v) || 0;
  else if (type === "floatnull") v = v.trim() === "" ? null : parseFloat(v);
  S.cfg[f] = v; delete S.errors[f]; showErrors();
  if (f === "layout" || f === "undated_layout" || f === "trip_layout") { renderPresets(); refreshPreview(); }
  if (f === "backup_dir") { refreshDest(); checkPath(); }
  if (f === "ai_host") debounce(loadOllama, 500)();
  updateSaveBar();
});
document.addEventListener("change", (ev) => { if (ev.target.id === "f-ai_model") { S.cfg.ai_model = ev.target.value; updateSaveBar(); } });

$("#themeBtn").addEventListener("click", () => {
  const cur = document.documentElement.dataset.theme || (matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light");
  const next = cur === "dark" ? "light" : "dark";
  document.documentElement.dataset.theme = next;
  try { localStorage.setItem("pv-theme", next); } catch (_) { /* storage unavailable */ }
});
$("#themeBtn").innerHTML = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M20 14.5A8 8 0 0 1 9.5 4a8 8 0 1 0 10.5 10.5z"/></svg>';
window.addEventListener("hashchange", route);
window.addEventListener("beforeunload", (e) => { if (dirty()) { e.preventDefault(); e.returnValue = ""; } });
window.addEventListener("focus", () => { if (S.page === "setup" && S.device && !S.device.connected) loadDevice(); });

/* ---------- boot ---------- */
(async function init() {
  renderNav(); setHeader(); renderConn(); route();
  const [cfg, vols, status] = await Promise.allSettled([api("/api/config"), api("/api/volumes"), api("/api/status")]);
  if (cfg.status === "rejected") { $("#page").innerHTML = '<div class="card empty"><h2>Cannot reach the app engine</h2><p>Close this tab and run <code>photovault ui</code> again.</p></div>'; return; }
  S.volumes = vols.value || []; S.status = status.value || { has_backup: false, counts: {} };
  S.cfg = cfg.value.config; S.cfgPath = cfg.value.path; S.cfgExists = cfg.value.exists;
  if (!S.cfgExists && !S.cfg.ssd_root) { const v = S.volumes.find((x) => !x.is_system) || S.volumes[0]; if (v) S.cfg.ssd_root = v.path; }
  S.saved = clone(S.cfg);
  if (!S.cfgExists) S.saved.ssd_root = cfg.value.config.ssd_root;  // a first-run suggestion counts as a change to confirm
  route(); renderConn(); checkPath(); loadDevice(); loadOllama(); connectEvents();
})();
