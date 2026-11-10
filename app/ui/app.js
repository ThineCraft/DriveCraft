// DriveCraft desktop interface. Everything it does goes through the commands in
// app/src/main.rs; when opened in an ordinary browser it talks to mock.js instead.
"use strict";

const TAURI = window.__TAURI__;
const invoke = TAURI ? TAURI.core.invoke : window.__DC_MOCK__.invoke;
const listen = TAURI ? TAURI.event.listen : window.__DC_MOCK__.listen;
const $ = (id) => document.getElementById(id);

let state = null;
let identity = null;
let selected = null;
let editingSetup = false;
let busyName = null;
const logs = {};
const results = {};

// Everything that differs between the three modes, in one place.
const MODES = {
  archive: {
    summary: "One-way download",
    help: "Download only. Changes in the library come here; nothing here is ever sent to the library.",
    localNote: "Receives changes only",
    arrow: "arrow-left",
    run: "Download now",
    icon: "run-icon-down",
    empty1: "Preview what will download before you begin.",
    empty2: "Nothing in the library is ever changed.",
    protect: "Library protected",
    localLabel: "On this computer",
  },
  sync: {
    summary: "Two-way sync",
    help: "Two-way. Changes here, including deletions, go to the library, and changes there come here.",
    localNote: "Changes go both ways",
    arrow: "arrow-both",
    run: "Sync now",
    icon: "run-icon-sync",
    empty1: "Preview what will change on both sides before you begin.",
    empty2: "Large deletions are held back until you confirm them.",
    protect: "Deletions are confirmed first",
    localLabel: "On this computer",
  },
  ingest: {
    summary: "One-way upload",
    help: "Uploads this folder into the library. The folder is only read; it is never changed.",
    localNote: "Source is read only",
    arrow: "arrow-right",
    run: "Upload now",
    icon: "run-icon-up",
    empty1: "Preview what will upload before you begin.",
    empty2: "Your source folder stays untouched.",
    protect: "Source protected",
    localLabel: "Uploaded from source",
  },
};

const CHIPS = [
  ["downloaded", "downloaded"],
  ["uploaded", "uploaded"],
  ["seeded", "items taken from a seed"],
  ["adopted", "already matching"],
  ["folders_created", "folders created here"],
  ["remote_folders_created", "folders created in the library"],
  ["renamed", "renamed here"],
  ["moved_remote", "moved in the library"],
  ["deleted_local", "removed here"],
  ["deleted_remote", "deleted from the library"],
  ["conflicts", "conflicts", "warn"],
  ["skipped", "skipped", "warn"],
  ["held_deletes", "deletions held back", "warn"],
  ["failed", "failed", "bad"],
];

function humanBytes(n) {
  const units = ["bytes", "KB", "MB", "GB", "TB"];
  let v = n, u = 0;
  while (v >= 1024 && u < units.length - 1) { v /= 1024; u++; }
  return u === 0 ? [String(n), "bytes"] : [v.toFixed(1), units[u]];
}

function relTime(unix) {
  if (!unix) return ["Never", ""];
  const secs = Math.max(0, Date.now() / 1000 - unix);
  if (secs < 60) return ["Just now", ""];
  if (secs < 3600) return [`${Math.floor(secs / 60)} min`, "ago"];
  if (secs < 86400) return [`${Math.floor(secs / 3600)} h`, "ago"];
  return [new Date(unix * 1000).toLocaleDateString(), ""];
}

function decoded(text) {
  try { return decodeURIComponent(text); } catch (e) { return text; }
}

// "C:\Users\me\OneDrive\Documents" -> "OneDrive / Documents"
function shortFolder(path) {
  const parts = path.split(/[\\/]/).filter(Boolean);
  return parts.slice(-2).join(" / ") || path;
}

function libraryParts(url) {
  try {
    const u = new URL(url);
    const segs = u.pathname.split("/").filter(Boolean).map(decoded);
    return { host: u.hostname.split(".")[0], name: segs.length ? segs[segs.length - 1] : u.hostname };
  } catch (e) {
    return { host: "", name: url };
  }
}

function el(tag, props = {}, ...children) {
  const node = document.createElement(tag);
  for (const [k, v] of Object.entries(props)) {
    if (k === "class") node.className = v;
    else if (k === "text") node.textContent = v;
    else if (k.startsWith("on")) node.addEventListener(k.slice(2), v);
    else node.setAttribute(k, v);
  }
  node.append(...children);
  return node;
}

let toastTimer = null;
function toast(message, good = false) {
  const t = $("toast");
  t.classList.toggle("good", good);
  t.textContent = String(message);
  t.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => { t.hidden = true; }, 9000);
}

function confirmBox(title, text, okLabel) {
  const dlg = $("confirm-dialog");
  $("confirm-title").textContent = title;
  $("confirm-text").textContent = text;
  $("confirm-ok").textContent = okLabel;
  dlg.returnValue = "cancel";
  dlg.showModal();
  return new Promise((resolve) => {
    dlg.addEventListener("close", () => resolve(dlg.returnValue === "ok"), { once: true });
  });
}

async function refresh() {
  try {
    state = await invoke("get_state");
  } catch (e) {
    toast(e);
    return;
  }
  if (state.signed_in && !identity) {
    invoke("whoami").then((who) => { identity = who; renderAccount(); }).catch(() => {});
  }
  if (!state.signed_in) identity = null;
  render();
}

function current() {
  return state.connections.find((c) => c.name === selected) || null;
}

function render() {
  const needSetup = !state.setup || editingSetup;
  const needSignin = !needSetup && !state.signed_in;
  const main = !needSetup && !needSignin;
  $("view-setup").hidden = !needSetup;
  $("view-signin").hidden = !needSignin;
  $("view-main").hidden = !main;
  $("topbar-right").hidden = !main;
  $("statusbar-signin").hidden = !main;
  $("auto-wrap").hidden = !main;
  $("net-wrap").hidden = !main;
  if (needSetup) {
    $("setup-cancel").hidden = !state.setup;
    if (state.setup && !$("setup-client").value) {
      $("setup-client").value = state.setup.client_id;
      $("setup-tenant").value = state.setup.tenant === "organizations" ? "" : state.setup.tenant;
    }
    return;
  }
  if (needSignin) return;

  if (!state.connections.some((c) => c.name === selected)) {
    selected = state.connections.length ? state.connections[0].name : null;
  }
  $("auto-sync").checked = state.auto_sync;
  $("version").textContent = `DriveCraft ${state.version}`;
  $("computer-os").textContent = state.is_windows ? "Windows" : navigator.platform || "";
  $("lib-count").textContent = String(state.connections.length);
  renderAccount();
  renderHealth();
  renderNetwork();
  renderList();
  renderDetail();
}

function renderAccount() {
  if (!state) return;
  const name = identity ? identity.name || identity.account : "Signed in";
  $("account-name").textContent = name;
  $("account-access").textContent = `Work account · ${state.can_write ? "Read and write" : "Read only"}`;
  $("avatar").textContent = (name.match(/\b\w/g) || ["?"]).slice(0, 2).join("").toUpperCase();
  $("menu-account").textContent = identity ? identity.account : "";
  $("menu-access").textContent = state.can_write ? "Switch to read-only access" : "Sign in with write access";
}

function renderHealth() {
  const h = $("health");
  if (state.is_windows && state.long_paths === false) {
    h.hidden = false;
    $("health-summary").textContent = "Long path support is off";
    $("health-text").textContent =
      "Files with paths over 260 characters still synchronize, but many programs cannot open them. " +
      "This is a Windows setting that this computer\u2019s administrator controls; DriveCraft does not change it.";
  } else {
    h.hidden = true;
  }
}

// Which network connection DriveCraft sends through, and the menu to change it.
function renderNetwork() {
  const link = state.link || {};
  const name = link.name || (link.address ? String(link.address) : "Unknown connection");
  $("net-label").textContent = link.address && link.name ? `${name} · ${link.address}` : name;
  $("net-button").classList.toggle("chosen", !!link.chosen);
  $("net-button").title = link.chosen
    ? "You chose this connection. DriveCraft sends everything through it."
    : "The system chose this connection. Click to pick a different one.";
  const pick = (address) => async () => {
    closePops();
    try { await invoke("set_network", { address }); } catch (e) { toast(e); }
    await refresh();
  };
  const row = (checked, label, addr, onclick) =>
    el("button", { role: "menuitemradio", "aria-checked": String(checked), onclick },
      el("span", { text: checked ? "\u2713" : "" }), el("span", { text: label }), el("span", { class: "addr", text: addr }));
  $("net-menu").replaceChildren(
    el("div", { class: "pop-title", text: "Send through" }),
    row(!link.chosen, "Let the system choose", "", pick("")),
    ...(state.links || []).map((l) => row(!!link.chosen && l.address === link.address, l.name, String(l.address), pick(String(l.address))))
  );
}

function isBusy(c) {
  return c.syncing || busyName === c.name;
}

function dotClass(c) {
  if (isBusy(c)) return "busy";
  if (!c.folder_exists && c.local_items > 0) return "bad";
  const r = results[c.name];
  if (r && (r.error || (r.report && r.report.failed > 0))) return "bad";
  if (r && r.report && (r.report.conflicts > 0 || r.report.held_deletes > 0)) return "warn";
  if (c.last_sync) return "ok";
  return "";
}

function renderList() {
  $("conn-list").replaceChildren(
    ...state.connections.map((c) =>
      el(
        "button",
        {
          class: "conn-item",
          "aria-current": String(c.name === selected),
          onclick: () => { selected = c.name; closePops(); renderList(); renderDetail(); },
        },
        el("span", { class: `dot ${dotClass(c)}`, "aria-hidden": "true" }),
        el("span", { class: "title", text: c.name }),
        el("span", { class: `badge ${c.mode}`, text: c.mode })
      )
    )
  );
}

function renderDetail() {
  const c = current();
  $("empty").hidden = !!c;
  $("conn").hidden = !c;
  if (!c) return;
  const busy = isBusy(c);
  const mode = MODES[c.mode];
  const lib = libraryParts(c.library_url);

  $("crumb-host").textContent = lib.host;
  $("conn-name").textContent = c.name;
  $("conn-sub").textContent = `${lib.name}  ·  ${mode.summary}`;

  $("flow-local-name").textContent = shortFolder(c.folder);
  $("conn-open-tile").title = `Open ${c.folder}`;
  $("flow-local-sub").textContent = mode.localNote;
  $("flow-remote-name").textContent = lib.name;
  $("conn-library").title = `Open ${decoded(c.library_url)} in your browser`;
  for (const id of ["arrow-right", "arrow-left", "arrow-both"]) $(id).toggleAttribute("hidden", id !== mode.arrow);
  $("conn-missing").hidden = c.folder_exists || c.local_items === 0;

  const fixed = c.mode === "ingest";
  $("mode-seg").hidden = fixed;
  $("mode-fixed").hidden = !fixed;
  for (const b of $("mode-seg").children) {
    b.setAttribute("aria-checked", String(b.dataset.mode === c.mode));
    b.disabled = busy;
  }
  $("mode-help").textContent = mode.help;
  $("update-wrap").hidden = !fixed;

  const scanned = c.first_scan !== "not started";
  const [size, unit] = humanBytes(c.remote_bytes);
  const [last, lastUnit] = relTime(c.last_sync);
  $("stat-files").textContent = scanned ? c.remote_files.toLocaleString() : "–";
  $("stat-size").textContent = scanned ? size : "–";
  $("stat-size-unit").textContent = scanned ? unit : "";
  $("stat-local").textContent = c.local_items.toLocaleString();
  $("stat-local-label").textContent = mode.localLabel;
  $("stat-last").textContent = last;
  $("stat-last-unit").textContent = lastUnit;

  $("run-sync").disabled = busy;
  $("run-preview").disabled = busy;
  $("run-sync-label").textContent = mode.run;
  for (const id of ["run-icon-up", "run-icon-down", "run-icon-sync"]) $(id).toggleAttribute("hidden", id !== mode.icon);
  $("run-status").textContent = busy
    ? "Working…"
    : c.first_scan === "interrupted"
      ? "The first scan was interrupted; it will resume"
      : "Ready";
  $("run-dot").className = `dot ${busy ? "busy" : "ok"}`;

  const note = $("readonly-note");
  note.hidden = c.mode === "archive" || state.can_write;
  note.textContent =
    "You are signed in read-only. Preview works, but this mode needs write access: use the account menu to sign in with write access.";

  $("log-empty-1").textContent = mode.empty1;
  $("log-empty-2").textContent = mode.empty2;
  $("foot-protect").textContent = mode.protect;

  renderResult(c);
  renderLog(c);
}

function renderResult(c) {
  const r = results[c.name];
  const box = $("result");
  const held = $("held");
  held.hidden = true;
  if (!r) { box.hidden = true; return; }
  box.hidden = false;
  const chips = $("result-chips");
  const warns = $("result-warnings");
  chips.replaceChildren();
  warns.replaceChildren();
  const details = r.details || [];
  $("result-details").hidden = details.length === 0;
  $("result-details-list").replaceChildren(...details.map((d) => el("li", { text: d })));

  if (r.error) {
    $("result-title").textContent = r.dry_run ? "The preview stopped" : "The run stopped";
    $("result-sub").textContent = "";
    warns.append(el("li", { class: "bad", text: r.error }));
    return;
  }
  const rep = r.report;
  $("result-title").textContent = r.dry_run ? "Preview: what would happen" : "Last run";
  const moved = rep.bytes_down + rep.bytes_up;
  const bits = [];
  if (moved > 0) bits.push(`${humanBytes(rep.bytes_down).join(" ")} down, ${humanBytes(rep.bytes_up).join(" ")} up`);
  if (r.throttled > 0) bits.push(`the server asked DriveCraft to slow down ${r.throttled} time(s); it waited`);
  if (r.dry_run) bits.push("nothing was changed");
  $("result-sub").textContent = bits.join(" · ");

  for (const [key, label, tone] of CHIPS) {
    if (rep[key] > 0) chips.append(el("span", { class: `chip ${tone || ""}`, text: `${rep[key].toLocaleString()} ${label}` }));
  }
  if (!chips.children.length) {
    chips.append(el("span", { class: "chip", text: r.dry_run ? "Nothing would change" : "Everything is up to date" }));
  }
  const shown = rep.warnings.slice(0, 40);
  for (const w of shown) warns.append(el("li", { class: w.startsWith("failed:") ? "bad" : "", text: w }));
  if (rep.warnings.length > shown.length) {
    warns.append(el("li", { text: `…and ${rep.warnings.length - shown.length} more. See Activity.` }));
  }

  if (rep.held_deletes > 0) {
    held.hidden = false;
    $("held-title").textContent = `${rep.held_deletes.toLocaleString()} items are missing from this folder.`;
  }
}

function renderLog(c) {
  const box = $("log");
  const lines = logs[c.name] || [];
  $("log-empty").hidden = lines.length > 0;
  box.hidden = lines.length === 0;
  box.replaceChildren(...lines.map((l) => el("div", { class: l.cls, text: l.text })));
  box.scrollTop = box.scrollHeight;
}

function pushLog(name, text, cls = "") {
  const list = (logs[name] = logs[name] || []);
  if (!cls) {
    if (text.startsWith("warning:")) cls = "warn";
    else if (text.startsWith("failed:")) cls = "bad";
  }
  list.push({ text, cls });
  if (list.length > 2000) list.splice(0, list.length - 2000);
  if (name === selected && !$("conn").hidden) {
    const box = $("log");
    $("log-empty").hidden = true;
    box.hidden = false;
    box.append(el("div", { class: cls, text }));
    while (box.childElementCount > 2000) box.firstElementChild.remove();
    box.scrollTop = box.scrollHeight;
  }
}

async function runSync(name, { dryRun = false, confirmDeletes = false } = {}) {
  if (busyName) return;
  busyName = name;
  renderList();
  renderDetail();
  try {
    await invoke("sync", { name, dryRun, confirmDeletes, update: $("run-update").checked });
  } catch (e) {
    toast(e);
  }
  busyName = null;
  await refresh();
}

async function signIn(readOnly, errorBox) {
  if (errorBox) errorBox.hidden = true;
  $("signin-wait").hidden = false;
  $("signin-button").disabled = true;
  try {
    identity = await invoke("login", { readOnly });
    await refresh();
  } catch (e) {
    if (errorBox) { errorBox.textContent = String(e); errorBox.hidden = false; }
    else toast(e);
  }
  $("signin-wait").hidden = true;
  $("signin-button").disabled = false;
}

// ---- wiring ----------------------------------------------------------------

function showTheme() {
  const choice = window.dcTheme.get();
  for (const b of $("theme").children) b.setAttribute("aria-checked", String(b.dataset.themeChoice === choice));
}
for (const b of $("theme").children) {
  b.addEventListener("click", () => { window.dcTheme.set(b.dataset.themeChoice); showTheme(); });
}
showTheme();

// The window has no title bar. Its one control puts it away in the system tray,
// where DriveCraft keeps running; the tray icon brings it back or quits.
if (TAURI) {
  $("win-min").hidden = false;
  $("win-min").addEventListener("click", () => invoke("hide_to_tray").catch(toast));
}

$("setup-form").addEventListener("submit", async (ev) => {
  ev.preventDefault();
  const err = $("setup-error");
  err.hidden = true;
  try {
    await invoke("save_setup", { clientId: $("setup-client").value, tenant: $("setup-tenant").value });
    editingSetup = false;
    identity = null;
    await refresh();
  } catch (e) {
    err.textContent = String(e);
    err.hidden = false;
  }
});
$("setup-cancel").addEventListener("click", () => { editingSetup = false; render(); });
$("signin-setup").addEventListener("click", () => { editingSetup = true; render(); });
$("signin-button").addEventListener("click", () => {
  const readOnly = document.querySelector("input[name=access]:checked").value === "read";
  signIn(readOnly, $("signin-error"));
});

// Pop-up menus: the account menu and the library settings.
const POPS = [["account-button", "account-menu"], ["settings-button", "settings-pop"], ["net-button", "net-menu"]];
function closePops(except) {
  for (const [button, pop] of POPS) {
    if (pop === except) continue;
    $(pop).hidden = true;
    $(button).setAttribute("aria-expanded", "false");
  }
}
for (const [button, pop] of POPS) {
  $(button).addEventListener("click", (ev) => {
    ev.stopPropagation();
    closePops(pop);
    $(pop).hidden = !$(pop).hidden;
    $(button).setAttribute("aria-expanded", String(!$(pop).hidden));
  });
  $(pop).addEventListener("click", (ev) => ev.stopPropagation());
}
document.addEventListener("click", () => closePops());
document.addEventListener("keydown", (ev) => { if (ev.key === "Escape") closePops(); });

$("menu-access").addEventListener("click", async () => {
  closePops();
  const toWrite = !state.can_write;
  if (toWrite) {
    const ok = await confirmBox(
      "Sign in with write access?",
      "Your browser will open so you can approve it. DriveCraft will then be able to upload, change and delete files in the libraries you connect in Sync or Ingest mode.",
      "Open browser"
    );
    if (!ok) return;
  }
  toast("Waiting for you to finish signing in in your browser…");
  await signIn(!toWrite, null);
  $("toast").hidden = true;
});
$("menu-setup").addEventListener("click", () => { closePops(); editingSetup = true; render(); });
$("menu-signout").addEventListener("click", async () => {
  closePops();
  try { await invoke("logout"); } catch (e) { toast(e); }
  identity = null;
  await refresh();
});

$("auto-sync").addEventListener("change", async (ev) => {
  try { await invoke("set_auto_sync", { enabled: ev.target.checked }); } catch (e) { toast(e); }
  await refresh();
});

$("open-logs").addEventListener("click", () => invoke("open_logs").catch(toast));
$("conn-library").addEventListener("click", () => invoke("open_connection", { name: selected, target: "library" }).catch(toast));
for (const id of ["conn-open", "conn-open-tile"]) {
  $(id).addEventListener("click", () => invoke("open_connection", { name: selected, target: "folder" }).catch(toast));
}
$("seed-publish").addEventListener("click", async () => {
  closePops();
  const c = current();
  const ok = await confirmBox(
    `Publish a seed for ${c.name}?`,
    "DriveCraft will store a snapshot of this library's file list in a .drivecraft folder in the library. Anyone who can open the library can see that folder. It lists file names and sizes, not the files themselves.",
    "Publish"
  );
  if (!ok) return;
  toast("Publishing the seed…", true);
  try {
    const done = await invoke("publish_seed", { name: c.name });
    toast(`Seed published: ${done.items.toLocaleString()} items, ${humanBytes(done.bytes).join(" ")}.`, true);
  } catch (e) {
    toast(e);
  }
});
$("conn-disconnect").addEventListener("click", async () => {
  closePops();
  const c = current();
  const ok = await confirmBox(
    `Disconnect ${c.name}?`,
    `DriveCraft will stop synchronizing this library. The files in ${c.folder} and in the library are left exactly as they are.`,
    "Disconnect"
  );
  if (!ok) return;
  try { await invoke("disconnect", { name: c.name }); } catch (e) { toast(e); }
  delete results[c.name];
  delete logs[c.name];
  await refresh();
});

for (const b of $("mode-seg").children) {
  b.addEventListener("click", async () => {
    const c = current();
    if (!c || b.dataset.mode === c.mode) return;
    closePops();
    if (b.dataset.mode === "sync") {
      const ok = await confirmBox(
        "Switch to two-way sync?",
        "From now on, changes you make in this folder are sent to the library. That includes deletions: a file you delete here is deleted from the library and goes to its recycle bin. Preview changes first to see what the next run would do.",
        "Switch to Sync"
      );
      if (!ok) return;
    }
    try { await invoke("set_mode", { name: c.name, mode: b.dataset.mode }); } catch (e) { toast(e); }
    delete results[c.name];
    await refresh();
  });
}

$("run-sync").addEventListener("click", () => runSync(selected));
$("run-preview").addEventListener("click", () => runSync(selected, { dryRun: true }));
$("held-confirm").addEventListener("click", async () => {
  const c = current();
  const n = results[c.name].report.held_deletes;
  const ok = await confirmBox(
    `Delete ${n.toLocaleString()} items from the library?`,
    "They will be removed from the library for everyone and moved to its recycle bin, where they can be restored for a limited time.",
    "Delete from the library"
  );
  if (ok) runSync(c.name, { confirmDeletes: true });
});

// Connect a library
const addDialog = $("add-dialog");
function addMode() { return document.querySelector("input[name=add-mode]:checked").value; }
function updateAddHints() {
  const mode = addMode();
  $("add-merge-wrap").hidden = mode !== "sync";
  $("add-readonly").hidden = mode === "archive" || state.can_write;
}
function openAdd() {
  closePops();
  $("add-form").reset();
  $("add-found").hidden = true;
  $("add-error").hidden = true;
  updateAddHints();
  addDialog.showModal();
  $("add-address").focus();
}
$("add-button").addEventListener("click", openAdd);
$("empty-add").addEventListener("click", openAdd);
$("add-cancel").addEventListener("click", () => addDialog.close());
document.querySelectorAll("input[name=add-mode]").forEach((r) => r.addEventListener("change", updateAddHints));
$("add-folder").addEventListener("input", () => {
  const parts = $("add-folder").value.split(/[\\/]/).filter(Boolean);
  $("add-name").placeholder = parts.length ? parts[parts.length - 1] : "Taken from the folder name";
});
$("add-browse").addEventListener("click", async () => {
  try {
    const picked = await invoke("pick_folder", { title: "Choose a folder for this library" });
    if (picked) {
      $("add-folder").value = picked;
      $("add-folder").dispatchEvent(new Event("input"));
    }
  } catch (e) { toast(e); }
});
$("add-check").addEventListener("click", async () => {
  const found = $("add-found");
  const err = $("add-error");
  found.hidden = true;
  err.hidden = true;
  $("add-check").disabled = true;
  try {
    const lib = await invoke("check_library", { address: $("add-address").value });
    found.textContent = `Found: ${lib.name} (${decoded(lib.url)})`;
    found.hidden = false;
  } catch (e) {
    err.textContent = String(e);
    err.hidden = false;
  }
  $("add-check").disabled = false;
});
$("add-form").addEventListener("submit", async (ev) => {
  ev.preventDefault();
  const err = $("add-error");
  err.hidden = true;
  $("add-submit").disabled = true;
  try {
    const name = await invoke("connect", {
      address: $("add-address").value,
      folder: $("add-folder").value,
      mode: addMode(),
      name: $("add-name").value,
      merge: $("add-merge").checked,
    });
    selected = name;
    addDialog.close();
    await refresh();
  } catch (e) {
    err.textContent = String(e);
    err.hidden = false;
  }
  $("add-submit").disabled = false;
});

// Events from the core
listen("sync-started", (ev) => {
  const name = ev.payload;
  pushLog(name, `── ${new Date().toLocaleTimeString()} ──`, "head");
  if (state) {
    const c = state.connections.find((x) => x.name === name);
    if (c) c.syncing = true;
    renderList();
    if (name === selected) renderDetail();
  }
});
listen("sync-log", (ev) => pushLog(ev.payload.name, ev.payload.line));
listen("sync-finished", (ev) => {
  const f = ev.payload;
  results[f.name] = f;
  if (f.error) pushLog(f.name, `stopped: ${f.error}`, "bad");
  refresh();
});

window.addEventListener("focus", () => { if (state && !busyName) refresh(); });
setInterval(() => { if (state && !$("view-main").hidden && !busyName) renderDetail(); }, 30000);

refresh();
