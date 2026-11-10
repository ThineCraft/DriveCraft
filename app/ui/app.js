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

// How each kind of problem is headed, and what the user can do about it.
const PROBLEM_KINDS = {
  stopped: ["The last run stopped", "DriveCraft could not finish. Fix the cause below, then try again."],
  failed: ["Could not be transferred", "These are tried again on every run. If one keeps failing, the reason beside it says why."],
  conflict: ["Changed in both places", "Nothing was overwritten. Decide which version you want, then synchronize again."],
  copy_kept: ["Your version was kept as a copy", "The library's version is in the original place and yours is beside it. Keep what you need and delete the other. This clears when the copy is gone."],
  not_uploaded: ["Not uploaded", "The library cannot take these as they are. Rename or move them and they go up on the next run."],
  held: ["Deletions waiting for you", "Put the files back, or synchronize and use \u201cDelete them from the library\u201d."],
  kept: ["Deleted in the library, kept here", "Someone deleted these in the library, but your copy holds changes. Delete it here, or move it somewhere else to keep it."],
  long_path: ["Long paths", "These synchronize normally. Some programs cannot open a path this long; shorten a folder name if that gets in your way."],
};

function tally(c) {
  return c.problems || { bad: 0, warn: 0, notice: 0 };
}

function problemsSummary(t) {
  const need = t.bad + t.warn;
  const bits = [];
  if (need) bits.push(`${need.toLocaleString()} ${need === 1 ? "problem needs" : "problems need"} your attention`);
  if (t.notice) bits.push(`${t.notice.toLocaleString()} ${t.notice === 1 ? "notice" : "notices"}`);
  return bits.join(" \u00b7 ");
}

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
  $("version").textContent = state.version;
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
  $("autostart-wrap").hidden = !main || !state.is_windows;
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
  $("autostart").checked = state.autostart;
  renderSyncButton();
  if (syncLinkWaiting) handleSyncLink();
  if (detailsWaiting) handleDetailsRequest();
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
  // Cut short when long; the whole address shows on hover.
  $("menu-account").title = identity ? identity.account : "";
  $("menu-access").textContent = state.can_write ? "Switch to read-only access" : "Sign in with write access";
}

function renderHealth() {
  const off = state.is_windows && state.long_paths === false;
  $("health").hidden = !state.is_windows;
  $("health").classList.toggle("warn", off);
  $("health-summary").textContent = off ? "Long path support is off"
    : state.long_paths === true ? "Long path support is on" : "Long path support could not be read";
  $("health-text").textContent = off
    ? "Files with paths over 260 characters still synchronize, but many programs cannot open them. " +
      "This is a Windows setting that this computer\u2019s administrator controls; DriveCraft does not change it."
    : "Files with paths over 260 characters synchronize either way. This Windows setting decides whether other programs can open them.";
  // The one thing in Settings worth pointing at without opening it.
  $("app-settings-dot").hidden = !off;
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
  const t = tally(c);
  if (t.bad > 0) return "bad";
  if (t.warn > 0) return "warn";
  if (c.last_sync) return "ok";
  return "";
}

// Pick a library. The panel sweeps in vertically, blurred as fast things are, and
// settles within half a second. It comes from below when the library is further
// down the list, and from above when it is further up.
let motionFrame = null;
function pick(name) {
  if (name === selected) return;
  const order = state.connections.map((c) => c.name);
  const from = order.indexOf(name) < order.indexOf(selected) ? -1 : 1;
  selected = name;
  closePops();
  renderList();
  renderDetail();
  sweepIn(from);
}
function sweepIn(from = 1) {
  const panel = $("conn");
  const amount = $("motion-amount");
  cancelAnimationFrame(motionFrame);
  if (panel.hidden || window.matchMedia("(prefers-reduced-motion: reduce)").matches) {
    panel.style.filter = panel.style.transform = panel.style.opacity = "";
    $("detail").style.overflow = "";
    return;
  }
  // The moving panel must not make its frame grow a scroll bar for half a second.
  const frame = $("detail");
  frame.style.overflow = "hidden";
  const start = performance.now();
  const step = (now) => {
    const t = Math.min(1, (now - start) / 500);
    const left = Math.pow(1 - t, 3); // fast at first, easing to rest
    if (t >= 1) {
      // Leave nothing behind: a filter would change how the panel's menus stack.
      panel.style.filter = panel.style.transform = panel.style.opacity = "";
      amount.setAttribute("stdDeviation", "0 0");
      frame.style.overflow = "";
      return;
    }
    amount.setAttribute("stdDeviation", `0 ${(22 * left).toFixed(2)}`);
    panel.style.filter = "url(#motion)";
    panel.style.transform = `translateY(${(from * 46 * left).toFixed(1)}px)`;
    panel.style.opacity = String(0.55 + 0.45 * t);
    motionFrame = requestAnimationFrame(step);
  };
  motionFrame = requestAnimationFrame(step);
}

// With many libraries the list scrolls, and a box above it narrows it down.
const MANY_LIBRARIES = 8;
function renderList() {
  const many = state.connections.length > MANY_LIBRARIES;
  $("conn-find-wrap").hidden = !many;
  const words = many ? $("conn-find").value.toLowerCase().split(/\s+/).filter(Boolean) : [];
  const shown = state.connections.filter((c) => {
    const hay = `${c.name} ${c.mode} ${decoded(c.library_url)}`.toLowerCase();
    return words.every((w) => hay.includes(w));
  });
  $("conn-none").hidden = shown.length > 0 || state.connections.length === 0;
  $("conn-list").replaceChildren(
    ...shown.map((c) => {
      const t = tally(c);
      const state_ = isBusy(c) ? "synchronizing" : t.bad ? `${t.bad} failing` : t.warn ? `${t.warn} to look at` : "";
      return el(
        "button",
        {
          class: "conn-item",
          "aria-current": String(c.name === selected),
          title: `${c.name} \u00b7 ${MODES[c.mode].summary}${state_ ? ` \u00b7 ${state_}` : ""}`,
          onclick: () => pick(c.name),
        },
        el("span", { class: `dot ${dotClass(c)}`, "aria-hidden": "true" }),
        el("span", { class: "title", text: c.name }),
        // In a long list the mode is a letter, so the name gets the room.
        el("span", { class: `badge ${c.mode}`, text: many ? c.mode[0].toUpperCase() : c.mode })
      );
    })
  );
  $("conn-list").classList.toggle("many", many);
  const current_ = $("conn-list").querySelector("[aria-current=true]");
  if (current_ && listJumped !== selected) {
    listJumped = selected;
    current_.scrollIntoView({ block: "nearest" });
  }
}
let listJumped = null;
$("conn-find").addEventListener("input", renderList);
$("conn-find").addEventListener("keydown", (ev) => {
  // Enter opens the first match, so a library is two keystrokes away.
  if (ev.key !== "Enter") return;
  const first = $("conn-list").querySelector(".conn-item");
  if (first) first.click();
});

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
  const t = tally(c);
  $("problems-bar").hidden = t.bad + t.warn + t.notice === 0;
  $("problems-bar").className = `problems-bar ${t.bad ? "bad" : t.warn ? "warn" : "notice"}`;
  $("problems-text").textContent = problemsSummary(t);
  const co = c.checked_out || { total: 0, mine: 0 };
  $("checkouts-bar").hidden = co.total === 0;
  $("checkouts-text").textContent =
    `${co.total.toLocaleString()} ${co.total === 1 ? "file is" : "files are"} checked out` +
    (co.mine ? ` \u00b7 ${co.mine === co.total ? (co.total === 1 ? "by you" : "all by you") : `${co.mine.toLocaleString()} by you`}` : "");

  const fixed = c.mode === "ingest";
  $("mode-seg").hidden = fixed;
  $("mode-fixed").hidden = !fixed;
  for (const b of $("mode-seg").children) {
    b.setAttribute("aria-checked", String(b.dataset.mode === c.mode));
    b.disabled = busy;
  }
  $("mode-help").textContent = mode.help;
  $("explorer-box").hidden = fixed || !state.is_windows;
  $("explorer-on").checked = !!c.explorer;
  $("explorer-on").disabled = busy || explorerBusy;
  $("explorer-more").hidden = !c.explorer;
  if (document.activeElement !== $("explorer-name")) $("explorer-name").value = c.explorer ? c.explorer.name : c.name;
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
  renderProgress();
}

// How long a run still has to go. The core reports where it is about once a second;
// between reports the clock here keeps counting down by itself.
const progressOf = {};
function spanWords(secs) {
  const s = Math.max(0, Math.round(secs));
  if (s < 60) return `${s} s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m} min ${String(s % 60).padStart(2, "0")} s`;
  const h = Math.floor(m / 60);
  if (h < 48) return `${h} h ${String(m % 60).padStart(2, "0")} min`;
  return `${Math.floor(h / 24)} days ${h % 24} h`;
}
// The estimate is there to answer "how long?" once and then be left alone. It is
// taken from the pace so far, counts down by the clock, and is only taken again
// every half hour, after a short "Recalculating…". The counts and the bar beside
// it stay live. It is always called an estimate, never a promise.
const ESTIMATE_EVERY = 30 * 60 * 1000;
const ESTIMATE_PAUSE = 10 * 1000;
const ESTIMATE_AFTER = 30 * 1000; // enough of a run to judge a pace by
const estimateOf = {};
function renderProgress() {
  for (const name of Object.keys(estimateOf)) if (!progressOf[name]) delete estimateOf[name];
  const p = selected && progressOf[selected];
  const c = selected && current();
  $("progress").hidden = !p || !c || !isBusy(c);
  if ($("progress").hidden) return;
  const doing = p.verb === "uploaded" ? "Uploaded" : "Downloaded";
  $("progress-fill").style.width = `${(100 * (p.total_bytes > 0 ? p.bytes / p.total_bytes : p.done / Math.max(1, p.total))).toFixed(2)}%`;
  $("progress-count").textContent =
    `${doing} ${p.done.toLocaleString()} of ${p.total.toLocaleString()} files · ${humanBytes(p.bytes).join(" ")} of ${humanBytes(p.total_bytes).join(" ")}`;
  $("progress-rate").textContent = p.files_per_second > 0
    ? `${p.files_per_second.toFixed(1)} files/s · ${humanBytes(Math.round(p.bytes_per_second)).join(" ")}/s` : "";

  const now = Date.now();
  const e = estimateOf[selected] || (estimateOf[selected] = { began: now, left: null, at: 0, pauseUntil: 0 });
  const known = p.seconds_left !== null && p.seconds_left !== undefined;
  const say = (left, until) => { $("progress-left").textContent = left; $("progress-until").textContent = until; };
  if (e.left === null) {
    // The first estimate waits until there is a pace worth going by.
    if (!known || now - e.began < ESTIMATE_AFTER) return say("Estimating…", "");
    Object.assign(e, { left: p.seconds_left, at: now });
  }
  let left = e.left - (now - e.at) / 1000;
  // Time for a fresh look: every half hour, when the old estimate has run out, or
  // when the pace has changed so much (three times faster or slower) that the old
  // estimate would mislead. That last check is made at most every five minutes.
  const wayOff = known && now - e.at >= 5 * 60 * 1000 && (p.seconds_left > left * 3 || p.seconds_left < left / 3);
  if (!e.pauseUntil && (now - e.at >= ESTIMATE_EVERY || left <= 0 || wayOff)) e.pauseUntil = now + ESTIMATE_PAUSE;
  if (e.pauseUntil) {
    if (now < e.pauseUntil || !known) return say("Recalculating…", "");
    Object.assign(e, { left: p.seconds_left, at: now, pauseUntil: 0 });
    left = e.left;
  }
  const end = new Date(now + left * 1000);
  const days = Math.floor((new Date(end).setHours(0, 0, 0, 0) - new Date().setHours(0, 0, 0, 0)) / 86400000);
  const day = days === 0 ? "" : days === 1 ? " tomorrow" : ` on ${end.toLocaleDateString([], { weekday: "long" })}`;
  say(`Estimated ${spanWords(left)} left`, `Estimated to finish around ${end.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" })}${day}`);
}
setInterval(() => { if (state && !$("view-main").hidden) renderProgress(); }, 1000);

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
    if (text.startsWith("… and ")) cls = "head";
    else if (text.startsWith("warning:")) cls = "warn";
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
  showScrim();
}
// The library settings panel gets the window behind it dimmed and blurred, like a dialog.
function showScrim() {
  $("scrim").hidden = $("settings-pop").hidden;
}
for (const [button, pop] of POPS) {
  $(button).addEventListener("click", (ev) => {
    ev.stopPropagation();
    closePops(pop);
    $(pop).hidden = !$(pop).hidden;
    $(button).setAttribute("aria-expanded", String(!$(pop).hidden));
    showScrim();
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

$("autostart").addEventListener("change", async (ev) => {
  try { await invoke("set_autostart", { enabled: ev.target.checked }); } catch (e) { toast(e); }
  await refresh();
});

$("auto-sync").addEventListener("change", async (ev) => {
  try { await invoke("set_auto_sync", { enabled: ev.target.checked }); } catch (e) { toast(e); }
  await refresh();
});

// Settings: a dialog, so a problem is shown inside it rather than behind it.
function settingsError(e) {
  $("app-settings-error").textContent = e ? String(e) : "";
  $("app-settings-error").hidden = !e;
}
$("app-settings-open").addEventListener("click", () => {
  closePops();
  settingsError(null);
  $("app-settings-dialog").showModal();
});
$("app-settings-close").addEventListener("click", () => $("app-settings-dialog").close());
$("open-logs").addEventListener("click", () => invoke("open_logs").then(() => settingsError(null), settingsError));
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

// File Explorer: list the folder there, under a name and icon for this computer.
let explorerBusy = false;
async function setExplorer(enabled, icon) {
  const c = current();
  if (!c || explorerBusy) return;
  const label = $("explorer-name").value;
  explorerBusy = true;
  renderDetail();
  $("explorer-name").value = label;
  if (!c.explorer || !enabled) toast(enabled ? "Adding the folder to File Explorer\u2026" : "Taking the folder out of File Explorer\u2026", true);
  try {
    const m = await invoke("set_explorer", { name: c.name, enabled, label, icon: icon || null });
    if (!enabled) toast("The folder is an ordinary folder again.", true);
    else if (!c.explorer) toast(`Shown in File Explorer. ${m.done.toLocaleString()} items have their status icon.`, true);
    else toast("File Explorer updated.", true);
  } catch (e) {
    toast(e);
  }
  explorerBusy = false;
  await refresh();
}
$("explorer-on").addEventListener("change", (ev) => setExplorer(ev.target.checked));
$("explorer-apply").addEventListener("click", () => setExplorer(true));
// The icon picker. The icons are Font Awesome Free's solid set, packed into the app;
// they are read the first time the picker opens. Whichever is chosen is drawn here
// at the sizes Windows asks for and handed to the core as an icon file.
const ICON_COLORS = [["blue", "#1f6fd6"], ["amber", "#d98a1f"], ["green", "#2e9e5b"], ["red", "#d0453a"], ["purple", "#7a56d6"], ["teal", "#178f9c"], ["slate", "#51606f"]];
// Shown first, before anything is searched for: what a library is most often about.
const ICON_FIRST = ["folder", "folder-open", "folder-tree", "briefcase", "building", "city", "landmark", "users", "people-group", "user-tie",
  "file-lines", "file-contract", "file-invoice-dollar", "file-signature", "book", "book-open", "box-archive", "boxes-stacked", "clipboard-list", "list-check",
  "chart-line", "chart-pie", "coins", "sack-dollar", "scale-balanced", "gavel", "shield-halved", "lock", "key", "handshake",
  "gear", "screwdriver-wrench", "helmet-safety", "industry", "truck", "ship", "plane", "house", "hospital", "graduation-cap",
  "flask", "leaf", "bolt", "globe", "map-location-dot", "camera", "image", "film", "palette", "code",
  "laptop", "server", "database", "cloud", "envelope", "phone", "comments", "bullhorn", "cart-shopping", "store",
  "star", "flag", "heart", "lightbulb", "rocket", "bullseye", "trophy", "calendar-days", "clock", "bell"];
let iconNames = null;
let iconPicked = null;
let iconColor = ICON_COLORS[0];
function loadIcons() {
  if (window.DC_FA) return Promise.resolve();
  return new Promise((resolve, reject) => {
    const s = el("script", { src: "fa-solid.js" });
    s.onload = resolve;
    s.onerror = () => reject("The icons could not be read.");
    document.head.append(s);
  });
}
function iconSvg(name) {
  const [w, h, shape] = window.DC_FA.icons[name];
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("viewBox", `0 0 ${w} ${h}`);
  svg.setAttribute("aria-hidden", "true");
  const path = document.createElementNS("http://www.w3.org/2000/svg", "path");
  path.setAttribute("d", shape);
  path.setAttribute("fill", "currentColor");
  svg.append(path);
  return svg;
}
function renderIcons() {
  const all = window.DC_FA.icons;
  if (!iconNames) {
    const first = ICON_FIRST.filter((n) => all[n]);
    iconNames = [...first, ...Object.keys(all).filter((n) => !first.includes(n))];
  }
  const words = $("icon-find").value.toLowerCase().split(/\s+/).filter(Boolean);
  const found = words.length
    ? iconNames.filter((n) => { const hay = `${n.replace(/-/g, " ")} ${all[n][3]}`; return words.every((w) => hay.includes(w)); })
    : iconNames;
  // A name that matches comes before one that only matches by a related word.
  if (words.length) found.sort((a, b) => Number(!words.every((w) => a.includes(w))) - Number(!words.every((w) => b.includes(w))));
  const shown = found.slice(0, 420);
  const grid = $("icon-grid");
  grid.style.color = iconColor[1];
  grid.replaceChildren(...shown.map((n) => {
    const b = el("button", { type: "button", role: "option", title: n.replace(/-/g, " "), "aria-selected": String(n === iconPicked), "data-icon": n });
    b.append(iconSvg(n));
    return b;
  }));
  $("icon-status").textContent = found.length === 0 ? "No icon matches. Try another word."
    : iconPicked ? `Chosen: ${iconPicked.replace(/-/g, " ")}`
    : `${found.length.toLocaleString()} icons` + (found.length > shown.length ? `, showing the first ${shown.length}. Search to narrow them.` : "");
  $("icon-use").disabled = !iconPicked;
}
$("icon-grid").addEventListener("click", (ev) => {
  const b = ev.target.closest("button[data-icon]");
  if (!b) return;
  iconPicked = b.dataset.icon;
  for (const x of $("icon-grid").children) x.setAttribute("aria-selected", String(x === b));
  $("icon-status").textContent = `Chosen: ${iconPicked.replace(/-/g, " ")}`;
  $("icon-use").disabled = false;
});
$("icon-grid").addEventListener("dblclick", (ev) => { if (ev.target.closest("button[data-icon]")) $("icon-use").click(); });
$("icon-find").addEventListener("input", renderIcons);
$("icon-colors").replaceChildren(...ICON_COLORS.map((c) => {
  const b = el("button", { type: "button", role: "radio", title: c[0], "aria-label": c[0], "aria-checked": String(c === iconColor), style: `background:${c[1]}` });
  b.addEventListener("click", () => {
    iconColor = c;
    for (const x of $("icon-colors").children) x.setAttribute("aria-checked", String(x === b));
    $("icon-grid").style.color = c[1];
  });
  return b;
}));
// An icon file holding the picture at each size Windows uses, drawn in the colour chosen.
async function drawIcon(name, color) {
  const [w, h, shape] = window.DC_FA.icons[name];
  const path = new Path2D(shape);
  const images = [];
  for (const size of [16, 20, 24, 32, 40, 48, 64, 256]) {
    const canvas = el("canvas", { width: size, height: size });
    const ctx = canvas.getContext("2d");
    const room = size * (size <= 24 ? 1 : 0.9);
    const scale = room / Math.max(w, h);
    ctx.translate((size - w * scale) / 2, (size - h * scale) / 2);
    ctx.scale(scale, scale);
    ctx.fillStyle = color;
    ctx.fill(path);
    const blob = await new Promise((r) => canvas.toBlob(r, "image/png"));
    images.push([size, new Uint8Array(await blob.arrayBuffer())]);
  }
  const head = 6 + 16 * images.length;
  const file = new Uint8Array(head + images.reduce((n, i) => n + i[1].length, 0));
  const view = new DataView(file.buffer);
  view.setUint16(2, 1, true);
  view.setUint16(4, images.length, true);
  let at = head;
  images.forEach(([size, png], i) => {
    const e = 6 + 16 * i;
    file[e] = file[e + 1] = size >= 256 ? 0 : size;
    view.setUint16(e + 4, 1, true);
    view.setUint16(e + 6, 32, true);
    view.setUint32(e + 8, png.length, true);
    view.setUint32(e + 12, at, true);
    file.set(png, at);
    at += png.length;
  });
  return file;
}
function iconError(e) {
  $("icon-error").textContent = e ? String(e) : "";
  $("icon-error").hidden = !e;
}
$("explorer-icon").addEventListener("click", async () => {
  closePops();
  iconPicked = null;
  iconError(null);
  $("icon-find").value = "";
  $("icon-grid").replaceChildren();
  $("icon-status").textContent = "Reading the icons\u2026";
  $("icon-dialog").showModal();
  $("icon-find").focus();
  try {
    await loadIcons();
    renderIcons();
  } catch (e) {
    $("icon-status").textContent = "";
    iconError(e);
  }
});
$("icon-cancel").addEventListener("click", () => $("icon-dialog").close());
$("icon-default").addEventListener("click", async () => {
  $("icon-dialog").close();
  await setExplorer(true, "default");
});
$("icon-use").addEventListener("click", async () => {
  if (!iconPicked) return;
  $("icon-use").disabled = true;
  try {
    const data = await drawIcon(iconPicked, iconColor[1]);
    const file = await invoke("save_icon", { key: `${iconPicked}-${iconColor[0]}`, data: Array.from(data) });
    $("icon-dialog").close();
    await setExplorer(true, `${file},0`);
  } catch (e) {
    iconError(e);
    $("icon-use").disabled = false;
  }
});

// A file's library details: its values in the columns the library defines.
let detailsData = null;
let detailsWaiting = true; // File Explorer may have started the app to show some
const detailsInputs = {};
function detailsError(e) {
  $("details-error").textContent = e ? String(e) : "";
  $("details-error").hidden = !e;
}
// A stored value as the text its input holds; the same form is used to tell what changed.
function detailsText(c) {
  const v = c.value;
  if (v === null || v === undefined) return "";
  if (c.kind === "yes_no") return v ? "yes" : "no";
  if (Array.isArray(v)) return v.join("; ");
  if (c.kind === "date") {
    const d = new Date(v);
    if (isNaN(d)) return "";
    const pad = (n) => String(n).padStart(2, "0");
    const day = `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
    return c.date_only ? day : `${day}T${pad(d.getHours())}:${pad(d.getMinutes())}`;
  }
  return String(v);
}
// What an input holds now, as the text the core expects.
function detailsTyped(c) {
  const input = detailsInputs[c.name];
  if (c.kind === "choice" && c.multi) return [...input.querySelectorAll("input:checked")].map((x) => x.value).join("; ");
  if (c.kind === "yes_no") return input.value;
  return input.value.trim();
}
function detailsToSend(c, typed) {
  if (c.kind !== "date" || !typed) return typed;
  // A day, or a day and time, as this computer's clock means it.
  const d = c.date_only ? new Date(`${typed}T00:00`) : new Date(typed);
  return isNaN(d) ? typed : d.toISOString();
}
function detailsChanges() {
  const out = {};
  for (const c of detailsData.columns) {
    if (c.editable && detailsTyped(c) !== detailsText(c)) out[c.name] = detailsToSend(c, detailsTyped(c));
  }
  return out;
}
function detailsTouched() {
  for (const c of detailsData.columns) {
    if (c.editable) detailsInputs[c.name].closest("label, fieldset").classList.toggle("changed", detailsTyped(c) !== detailsText(c));
  }
  $("details-save").disabled = Object.keys(detailsChanges()).length === 0;
}
function renderDetails() {
  const d = detailsData;
  const name = d.path.split("/").pop();
  $("details-title").textContent = name;
  const folder = d.path.includes("/") ? d.path.slice(0, d.path.lastIndexOf("/")) : "Top of the library";
  $("details-sub").textContent = `${d.connection} \u00b7 ${folder}` + (d.modified_by ? ` \u00b7 last changed by ${d.modified_by}` : "");
  $("details-status").textContent = d.columns.some((c) => c.editable) ? "" : "This library has no details that can be changed here.";
  const box = $("details-fields");
  box.replaceChildren();
  for (const c of d.columns) {
    const title = el("span", { text: c.title });
    if (c.required && c.editable) title.append(el("span", { class: "req", text: " *", title: "Must have a value" }));
    const now = detailsText(c);
    let input;
    if (!c.editable) {
      const shown = c.kind === "date" && c.value ? new Date(c.value).toLocaleString() : now;
      box.append(el("label", {}, title, el("div", { class: "fixed", text: shown || "\u2014" })));
      continue;
    }
    if (c.kind === "choice" && c.multi) {
      input = el("div", { class: "picks" }, ...c.choices.map((ch) => {
        const tick = el("input", { type: "checkbox", value: ch });
        tick.checked = Array.isArray(c.value) && c.value.includes(ch);
        return el("label", { class: "check" }, tick, ` ${ch}`);
      }));
      detailsInputs[c.name] = input;
      box.append(el("fieldset", {}, el("legend", {}, title), input));
      continue;
    }
    if (c.kind === "choice") {
      const options = c.choices.includes(now) || !now ? c.choices : [now, ...c.choices];
      input = el("select", {}, el("option", { value: "", text: c.required ? "Choose\u2026" : "(none)" }), ...options.map((ch) => el("option", { value: ch, text: ch })));
    } else if (c.kind === "yes_no") {
      input = el("select", {}, ...(c.value === null || c.value === undefined ? [el("option", { value: "", text: "(not set)" })] : []),
        el("option", { value: "yes", text: "Yes" }), el("option", { value: "no", text: "No" }));
    } else if (c.kind === "note") {
      input = el("textarea", { spellcheck: "true" });
    } else {
      input = el("input", { type: c.kind === "number" ? "number" : c.kind === "date" ? (c.date_only ? "date" : "datetime-local") : "text", autocomplete: "off" });
      if (c.kind === "number") input.setAttribute("step", "any");
    }
    input.value = now;
    detailsInputs[c.name] = input;
    const label = el("label", {}, title, input);
    if (c.description) label.append(el("span", { class: "muted small", text: c.description }));
    box.append(label);
  }
  detailsTouched();
}
async function openDetails(file) {
  for (const d of document.querySelectorAll("dialog[open]")) d.close();
  detailsData = null;
  detailsError(null);
  $("details-title").textContent = file.split(/[\\/]/).pop();
  $("details-sub").textContent = "";
  $("details-status").textContent = "Asking the server\u2026";
  $("details-fields").replaceChildren();
  $("details-save").disabled = true;
  $("details-web").hidden = true;
  $("details-dialog").showModal();
  try {
    detailsData = await invoke("file_details", { file });
    // Show the file's own library behind the panel, whichever was open before.
    if (state.connections.some((c) => c.name === detailsData.connection) && selected !== detailsData.connection) {
      selected = detailsData.connection;
      renderList();
      renderDetail();
    }
    $("details-web").hidden = false;
    renderDetails();
  } catch (e) {
    $("details-status").textContent = "";
    detailsError(e);
  }
}
$("details-fields").addEventListener("input", () => { if (detailsData) detailsTouched(); });
$("details-fields").addEventListener("change", () => { if (detailsData) detailsTouched(); });
$("details-form").addEventListener("submit", async (ev) => {
  ev.preventDefault();
  if (!detailsData) return;
  const changes = detailsChanges();
  if (!Object.keys(changes).length) return;
  $("details-save").disabled = true;
  $("details-save").textContent = "Saving\u2026";
  try {
    detailsData = await invoke("save_details", { file: detailsData.file, changes });
    detailsError(null);
    renderDetails();
    $("details-status").textContent = "Saved to the library.";
  } catch (e) {
    detailsError(e);
    $("details-save").disabled = false;
  }
  $("details-save").textContent = "Save";
});
$("details-close").addEventListener("click", () => $("details-dialog").close());
$("details-web").addEventListener("click", () => invoke("open_in_library", { file: detailsData.file }).then(() => detailsError(null), detailsError));
$("details-open").addEventListener("click", async () => {
  closePops();
  try {
    const file = await invoke("pick_library_file", { name: selected });
    if (file) openDetails(file);
  } catch (e) { toast(e); }
});
async function handleDetailsRequest() {
  if ($("view-main").hidden) return;
  detailsWaiting = false;
  const file = await invoke("take_details_request").catch(() => null);
  if (file) openDetails(file);
}
listen("details", () => { detailsWaiting = true; handleDetailsRequest(); });

// The recycle bin of the selected library's site. The whole list is fetched once,
// newest deletions first, and kept for the session, so finding something is a matter
// of typing: every search and filter runs here, on what has already arrived.
const binCache = {};
let binFor = null;
let binShown = 100;
const binPicked = new Set();
const BIN_KINDS = {
  doc: ["doc", "docx", "docm", "dot", "dotx", "odt", "rtf", "txt", "md", "one", "msg", "eml"],
  sheet: ["xls", "xlsx", "xlsm", "xlsb", "csv", "ods"],
  slides: ["ppt", "pptx", "pptm", "pps", "ppsx", "odp"],
  pdf: ["pdf"],
  image: ["png", "jpg", "jpeg", "gif", "bmp", "tif", "tiff", "svg", "webp", "heic", "psd", "ai"],
  media: ["mp4", "mov", "avi", "mkv", "wmv", "mp3", "wav", "m4a", "flac", "aac"],
  zip: ["zip", "7z", "rar", "gz", "tar", "iso"],
};
function binKind(name) {
  const dot = name.lastIndexOf(".");
  if (dot <= 0 || dot === name.length - 1 || name.length - dot > 8) return "none";
  const ext = name.slice(dot + 1).toLowerCase();
  for (const [kind, list] of Object.entries(BIN_KINDS)) if (list.includes(ext)) return kind;
  return "other";
}
function binError(e) {
  $("bin-error").textContent = e ? String(e) : "";
  $("bin-error").hidden = !e;
}
// Get an item ready for searching, once.
function binPrepare(b, lib) {
  b.inLib = b.from === lib || b.from.startsWith(lib + "/");
  b.where = b.from === lib ? "Top of the library" : b.inLib ? b.from.slice(lib.length + 1) : b.from || "Elsewhere in the site";
  b.hay = `${b.name} ${b.where} ${b.deleted_by}`.toLowerCase();
  b.at = b.deleted_at ? Date.parse(b.deleted_at) : 0;
  b.kind = binKind(b.name);
  return b;
}
// Fetch the list page by page. What has arrived is searchable straight away.
async function binLoad(name) {
  const old = binCache[name];
  if (old && old.loading) return;
  const fresh = { items: [], loading: true, library_path: old ? old.library_path : null, at: Date.now() };
  if (old) old.loading = true; else binCache[name] = fresh;
  if (binFor === name) renderBin();
  try {
    let next = null;
    do {
      const page = await invoke("bin_page", { name, libraryPath: fresh.library_path, next });
      fresh.library_path = page.library_path;
      for (const b of page.items) fresh.items.push(binPrepare(b, page.library_path));
      next = page.next;
      // The first time, show each page as it comes; a refresh swaps in when complete.
      if (!old && binFor === name) renderBin();
    } while (next);
    fresh.loading = false;
    binCache[name] = fresh;
    if (binFor === name) binError(null);
  } catch (e) {
    fresh.loading = false;
    if (old) old.loading = false;
    if (binFor === name) binError(e);
  }
  if (binFor === name) renderBin();
}
function binFilters() {
  return {
    words: $("bin-filter").value.toLowerCase().split(/\s+/).filter(Boolean),
    when: Number($("bin-when").value || 0),
    who: $("bin-who").value,
    kind: $("bin-kind").value,
    site: $("bin-scope").value === "site",
    sort: $("bin-sort").value,
  };
}
function binMatches(data, f) {
  const startOfToday = new Date().setHours(0, 0, 0, 0);
  const since = f.when > 0 ? startOfToday - (f.when - 1) * 86400000 : 0;
  const before = f.when < 0 ? Date.now() + f.when * 86400000 : 0;
  const out = data.items.filter((b) =>
    (f.site || b.inLib) &&
    (!since || b.at >= since) &&
    (!before || b.at < before) &&
    (!f.who || b.deleted_by === f.who) &&
    (!f.kind || b.kind === f.kind) &&
    f.words.every((w) => b.hay.includes(w)));
  if (f.sort === "old") out.sort((a, b) => a.at - b.at);
  else if (f.sort === "name") out.sort((a, b) => a.name.localeCompare(b.name));
  else if (f.sort === "big") out.sort((a, b) => b.size - a.size);
  else out.sort((a, b) => b.at - a.at);
  return out;
}
// The text with the searched words marked.
function binMarked(text, words) {
  const node = document.createDocumentFragment();
  const lower = text.toLowerCase();
  const hits = [];
  for (const w of words) {
    for (let i = lower.indexOf(w); i !== -1; i = lower.indexOf(w, i + w.length)) hits.push([i, i + w.length]);
  }
  hits.sort((a, b) => a[0] - b[0]);
  let at = 0;
  for (const [from, to] of hits) {
    if (from < at) continue;
    node.append(text.slice(at, from), el("mark", { text: text.slice(from, to) }));
    at = to;
  }
  node.append(text.slice(at));
  return node;
}
function binWhen(at) {
  if (!at) return "";
  const d = new Date(at);
  const days = Math.floor((new Date().setHours(0, 0, 0, 0) - new Date(at).setHours(0, 0, 0, 0)) / 86400000);
  const time = d.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });
  if (days === 0) return `Today ${time}`;
  if (days === 1) return `Yesterday ${time}`;
  return d.toLocaleDateString([], { year: "numeric", month: "short", day: "numeric" });
}
function renderBin() {
  const data = binCache[binFor];
  const box = $("bin-list");
  if (!data) { box.replaceChildren(); return; }
  const f = binFilters();

  // Who deleted things, most frequent first, for the "deleted by" filter.
  const people = new Map();
  for (const b of data.items) if (b.deleted_by) people.set(b.deleted_by, (people.get(b.deleted_by) || 0) + 1);
  const names = [...people.entries()].sort((a, b) => b[1] - a[1]).map((p) => p[0]);
  const whoBox = $("bin-who");
  if (whoBox.dataset.names !== names.join("|")) {
    whoBox.dataset.names = names.join("|");
    whoBox.replaceChildren(el("option", { value: "", text: "Deleted by anyone" }),
      ...names.map((n) => el("option", { value: n, text: identity && n === identity.name ? `${n} (you)` : n })));
    whoBox.value = names.includes(f.who) ? f.who : "";
    f.who = whoBox.value;
  }
  for (const id of ["bin-when", "bin-who", "bin-kind"]) $(id).classList.toggle("on", !!$(id).value);
  $("bin-scope").classList.toggle("on", f.site);
  $("bin-clear").hidden = !(f.words.length || f.when || f.who || f.kind);

  const found = binMatches(data, f);
  const total = data.items.filter((b) => f.site || b.inLib).length;
  const shown = found.slice(0, binShown);
  const place = f.site ? "the site’s recycle bin" : "the recycle bin from this library";
  let status;
  if (total === 0) status = data.loading ? "Asking the server…" : `Nothing is in ${place}.`;
  else if (found.length === total) status = `${total.toLocaleString()} ${total === 1 ? "item" : "items"} in ${place}`;
  else status = `${found.length.toLocaleString()} of ${total.toLocaleString()} match`;
  if (data.loading && total > 0) status += " · still loading older deletions…";
  $("bin-status").textContent = status;

  for (const id of [...binPicked]) if (!data.items.some((b) => b.id === id)) binPicked.delete(id);
  box.replaceChildren(...shown.map((b) => {
    const tick = el("input", { type: "checkbox", "aria-label": `Select ${b.name}` });
    tick.checked = binPicked.has(b.id);
    tick.addEventListener("change", () => {
      if (tick.checked) binPicked.add(b.id); else binPicked.delete(b.id);
      renderBinActions();
    });
    const name = el("div", { class: "prow-path" });
    name.append(binMarked(b.name, f.words));
    const where = el("div", { class: "who" });
    where.append(binMarked(b.where, f.words));
    const by = el("div", {});
    by.append(binMarked(b.deleted_by || "", f.words));
    return el("label", { class: "brow" }, tick,
      el("div", { class: "prow-main" }, name, where),
      el("div", { class: "meta" }, el("div", { text: `${binWhen(b.at)} · ${humanBytes(b.size).join(" ")}` }), by));
  }));
  if (found.length > shown.length) {
    box.append(el("button", { class: "btn quiet bin-more", text: `Show ${Math.min(200, found.length - shown.length).toLocaleString()} more`,
      onclick: () => { binShown += 200; renderBin(); } }));
  }
  if (total > 0 && found.length === 0) {
    box.append(el("p", { class: "muted", text: f.site ? "Nothing matches. Try fewer words or clear the filters." : "Nothing matches in this library. Try fewer words, or look in the whole site." }));
  }
  renderBinActions();
}
function renderBinActions() {
  const n = binPicked.size;
  $("bin-restore").hidden = n === 0;
  $("bin-restore").textContent = n === 1 ? "Restore 1 item" : `Restore ${n.toLocaleString()} items`;
}
let binTyping = null;
function binChanged() {
  binShown = 100;
  clearTimeout(binTyping);
  binTyping = setTimeout(renderBin, 60);
}
$("bin-filter").addEventListener("input", binChanged);
for (const id of ["bin-when", "bin-who", "bin-kind", "bin-scope", "bin-sort"]) $(id).addEventListener("change", binChanged);
$("bin-clear").addEventListener("click", () => {
  $("bin-filter").value = "";
  for (const id of ["bin-when", "bin-who", "bin-kind"]) $(id).value = "";
  binChanged();
  $("bin-filter").focus();
});
$("bin-open").addEventListener("click", () => {
  closePops();
  binFor = selected;
  binShown = 100;
  binPicked.clear();
  $("bin-title").textContent = `Recycle bin: ${binFor}`;
  $("bin-filter").value = "";
  for (const id of ["bin-when", "bin-who", "bin-kind"]) $(id).value = "";
  $("bin-scope").value = "lib";
  $("bin-sort").value = "new";
  $("bin-sync").hidden = true;
  binError(null);
  $("bin-dialog").showModal();
  $("bin-filter").focus();
  renderBin();
  // What was fetched before shows at once; the server is asked again behind it.
  binLoad(binFor);
});
$("bin-reload").addEventListener("click", () => binLoad(binFor));
$("bin-restore").addEventListener("click", async () => {
  const name = binFor;
  const ids = [...binPicked];
  const button = $("bin-restore");
  button.disabled = true;
  button.textContent = "Restoring…";
  try {
    await invoke("restore_bin", { name, ids });
    binError(null);
    const data = binCache[name];
    if (data) data.items = data.items.filter((b) => !binPicked.has(b.id));
    binPicked.clear();
    $("bin-sync").hidden = false;
  } catch (e) {
    binError(e);
  }
  button.disabled = false;
  if (binFor === name) renderBin();
});
$("bin-close").addEventListener("click", () => $("bin-dialog").close());
$("bin-dialog").addEventListener("close", () => { binFor = null; });
$("bin-sync").addEventListener("click", () => {
  const name = binFor;
  $("bin-dialog").close();
  runSync(name);
});

// Checked-out files in the selected library
let checkoutsFor = null;
function checkoutsError(e) {
  $("checkouts-error").textContent = e ? String(e) : "";
  $("checkouts-error").hidden = !e;
}
async function loadCheckouts(askServer) {
  let list;
  try {
    list = await invoke("list_checkouts", { name: checkoutsFor, askServer });
    checkoutsError(null);
  } catch (e) {
    checkoutsError(e);
    if (!askServer) return;
    try { list = await invoke("list_checkouts", { name: checkoutsFor, askServer: false }); } catch (e2) { return; }
  }
  $("checkouts-title").textContent = `Checked out in ${checkoutsFor}`;
  $("checkouts-none").hidden = list.length > 0;
  $("checkouts-comment-wrap").hidden = !list.some((f) => f.mine && f.path);
  const box = $("checkouts-list");
  box.hidden = list.length === 0;
  box.replaceChildren(...list.slice(0, 300).map(checkoutRow));
  if (list.length > 300) box.append(el("p", { class: "muted small", text: `\u2026and ${(list.length - 300).toLocaleString()} more.` }));
}
function checkoutRow(f) {
  const name = checkoutsFor;
  const actions = el("span", { class: "prow-actions" });
  if (f.mine && f.path) {
    actions.append(el("button", { class: "textlink small", text: "Check in", onclick: async (ev) => {
      ev.target.disabled = true;
      try {
        await invoke("check_file", { name, path: f.path, out: false, comment: $("checkouts-comment").value });
      } catch (e) { checkoutsError(e); }
      if (checkoutsFor === name) await loadCheckouts(false);
      refresh();
    } }));
  }
  if (f.path) {
    actions.append(el("button", { class: "textlink small", text: "Show in folder",
      onclick: () => invoke("show_problem", { name, path: f.path }).then(() => checkoutsError(null), checkoutsError) }));
  }
  const who = f.mine ? "you" : f.by || "someone";
  return el("div", { class: `prow ${f.mine ? "mine" : ""}` },
    el("div", { class: "prow-main" },
      el("div", { class: "prow-path", text: f.path || "A file that is not on this computer" }),
      el("div", { class: "who", text: `Checked out by ${who} \u00b7 first seen ${new Date(f.since * 1000).toLocaleString()}` })),
    actions);
}
$("checkouts-bar").addEventListener("click", async () => {
  closePops();
  checkoutsFor = selected;
  $("checkouts-list").replaceChildren();
  $("checkouts-comment").value = "";
  await loadCheckouts(false);
  $("checkouts-dialog").showModal();
  loadCheckouts(true);
});
$("checkouts-refresh").addEventListener("click", () => loadCheckouts(true).then(refresh));
$("checkouts-close").addEventListener("click", () => $("checkouts-dialog").close());
$("checkouts-dialog").addEventListener("close", () => { checkoutsFor = null; });

// Problems in the selected library
let problemsFor = null;
function problemsError(e) {
  $("problems-error").textContent = e ? String(e) : "";
  $("problems-error").hidden = !e;
}
async function loadProblems() {
  let list;
  try {
    list = await invoke("list_problems", { name: problemsFor });
    problemsError(null);
  } catch (e) {
    problemsError(e);
    return;
  }
  $("problems-title").textContent = `Problems in ${problemsFor}`;
  $("problems-none").hidden = list.length > 0;
  $("problems-dismiss-all").hidden = !list.some((p) => p.lasting);
  $("problems-retry").hidden = !list.some((p) => !p.lasting);
  const groups = new Map();
  for (const p of list) {
    if (!groups.has(p.kind)) groups.set(p.kind, []);
    groups.get(p.kind).push(p);
  }
  const box = $("problems-list");
  box.hidden = list.length === 0;
  box.replaceChildren();
  for (const [kind, items] of groups) {
    const [title, help] = PROBLEM_KINDS[kind] || [kind, ""];
    const level = items[0].level === "notice" ? "" : items[0].level;
    const sec = el("section", { class: "pgroup" },
      el("h3", {}, el("span", { class: `dot ${level}`, "aria-hidden": "true" }), el("span", { text: title }),
        el("span", { class: "count", text: items.length.toLocaleString() })),
      el("p", { class: "muted small", text: help }));
    for (const p of items.slice(0, 200)) sec.append(problemRow(p));
    if (items.length > 200) sec.append(el("p", { class: "muted small", text: `\u2026and ${(items.length - 200).toLocaleString()} more of this kind.` }));
    box.append(sec);
  }
}
function problemRow(p) {
  const actions = el("span", { class: "prow-actions" });
  if (p.path) {
    actions.append(el("button", { class: "textlink small", text: "Show in folder",
      onclick: () => invoke("show_problem", { name: problemsFor, path: p.path }).then(() => problemsError(null), problemsError) }));
  }
  if (p.lasting) {
    actions.append(el("button", { class: "textlink small", text: "Dismiss", onclick: () => dismissProblems(p.kind, p.path) }));
  }
  const main = el("div", { class: "prow-main" });
  if (p.path) main.append(el("div", { class: "prow-path", text: p.path }));
  main.append(el("div", { class: "prow-msg", text: p.message }),
    el("div", { class: "muted small", text: `Since ${new Date(p.first_seen * 1000).toLocaleString()}` }));
  return el("div", { class: "prow" }, main, actions);
}
async function dismissProblems(kind, path) {
  try {
    await invoke("dismiss_problems", { name: problemsFor, kind: kind || null, path: path || null });
  } catch (e) {
    problemsError(e);
    return;
  }
  await loadProblems();
  refresh();
}
$("problems-bar").addEventListener("click", async () => {
  closePops();
  problemsFor = selected;
  $("problems-list").replaceChildren();
  await loadProblems();
  $("problems-dialog").showModal();
});
$("problems-close").addEventListener("click", () => $("problems-dialog").close());
$("problems-dialog").addEventListener("close", () => { problemsFor = null; });
$("problems-dismiss-all").addEventListener("click", () => dismissProblems());
$("problems-retry").addEventListener("click", () => {
  const name = problemsFor;
  $("problems-dialog").close();
  runSync(name);
});

// The browser's Sync button: who answers it, and what to do when it is pressed.
let syncLinkWaiting = true; // one may have started the app
let addLink = null;
function renderSyncButton() {
  const sb = state.sync_button || { ours: false, other: null };
  $("syncbtn-wrap").hidden = !state.is_windows;
  $("syncbtn").checked = sb.ours;
  $("syncbtn-note").textContent = sb.ours
    ? `Sync in your browser opens DriveCraft. Turn this off to give it back${sb.other ? ` to ${sb.other}` : ""}, exactly as it was.`
    : sb.other ? `Sync in your browser opens ${sb.other}.` : "No program answers Sync in your browser.";
}
$("syncbtn").addEventListener("change", async (ev) => {
  const on = ev.target.checked;
  const other = (state.sync_button && state.sync_button.other) || null;
  if (on) {
    const ok = await confirmBox(
      "Let DriveCraft answer the Sync button?",
      `From now on, pressing Sync on a library in your browser opens DriveCraft${other ? ` instead of ${other}` : ""}. ` +
      `${other || "OneDrive"} is not removed or changed, and whatever it already synchronizes keeps synchronizing. ` +
      "DriveCraft writes down the current setting first; turning this switch off puts it back exactly as it was. " +
      "Each time, DriveCraft asks before connecting anything, and you can still send that one library to the other program.",
      "Take over the Sync button"
    );
    if (!ok) { ev.target.checked = false; return; }
  }
  try {
    await invoke("set_sync_button", { enabled: on });
    settingsError(null);
  } catch (e) { settingsError(e); }
  // The line under the switch says who has the button now.
  await refresh();
});
async function handleSyncLink() {
  if ($("view-main").hidden) return; // set up and sign in first; the link waits
  syncLinkWaiting = false;
  let link;
  try {
    link = await invoke("take_sync_link");
  } catch (e) {
    toast(`DriveCraft could not use the Sync link from your browser. ${e}`);
    return;
  }
  if (link) showSyncLink(link);
}
function showSyncLink(link) {
  if (link.existing) {
    selected = link.existing;
    render();
    toast(`${link.library_title} is already connected here, as ${link.existing}.`, true);
    return;
  }
  for (const d of document.querySelectorAll("dialog[open]")) d.close();
  openAdd();
  addLink = link;
  $("add-address").value = link.address;
  $("add-folder").value = link.suggested_folder;
  $("add-name").value = link.suggested_name;
  document.querySelector("input[name=add-mode][value=sync]").checked = true;
  updateAddHints();
  $("add-from").hidden = false;
  $("add-from-title").textContent = `From your browser: ${link.library_title}${link.site_title ? ` in ${link.site_title}` : ""}`;
  const notes = ["Check the folder and the mode, then Connect. Nothing is synchronized until you do."];
  if (link.folder) notes.push("You pressed Sync inside a folder; DriveCraft connects the whole library.");
  if (link.other_account) notes.push(`Your browser is signed in as ${link.other_account}, which is not the account DriveCraft uses.`);
  $("add-from-text").textContent = notes.join(" ");
  $("add-pass-on").hidden = !link.other;
  $("add-pass-on").textContent = `Use ${link.other} for this one`;
  $("add-folder").focus();
}
$("add-pass-on").addEventListener("click", async () => {
  const link = addLink;
  try {
    await invoke("pass_on_sync_link", { url: link.url });
    addDialog.close();
    toast(`Handed to ${link.other}.`, true);
  } catch (e) {
    $("add-error").textContent = String(e);
    $("add-error").hidden = false;
  }
});
listen("sync-link", () => { syncLinkWaiting = true; refresh(); });

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
  addLink = null;
  $("add-from").hidden = true;
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
listen("sync-progress", (ev) => {
  progressOf[ev.payload.name] = Object.assign({ at: Date.now() }, ev.payload);
  if (ev.payload.name === selected) renderProgress();
});
$("brand-link").addEventListener("click", () => invoke("open_site").catch(toast));
listen("notice", (ev) => {
  // Inside an open dialog a toast would be hidden behind it.
  if (checkoutsFor) { if (!ev.payload.good) checkoutsError(ev.payload.text); }
  else toast(ev.payload.text, ev.payload.good);
  refresh();
});
listen("sync-finished", (ev) => {
  const f = ev.payload;
  delete progressOf[f.name];
  results[f.name] = f;
  if (f.error) pushLog(f.name, `stopped: ${f.error}`, "bad");
  if (problemsFor === f.name) loadProblems();
  refresh();
});

window.addEventListener("focus", () => { if (state && !busyName) refresh(); });
setInterval(() => { if (state && !$("view-main").hidden && !busyName) renderDetail(); }, 30000);

refresh();
