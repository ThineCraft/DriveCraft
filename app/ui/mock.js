// A stand-in for the DriveCraft core, used only when this page is opened in an
// ordinary browser for design work. Inside the app, window.__TAURI__ exists and this
// file does nothing. Add ?view=setup, ?view=signin or ?view=empty to see those screens.
"use strict";

(function () {
  if (window.__TAURI__) return;
  const view = new URLSearchParams(location.search).get("view") || "main";
  const now = Math.floor(Date.now() / 1000);
  const listeners = {};
  const emit = (name, payload) => (listeners[name] || []).forEach((fn) => fn({ payload }));
  const wait = (ms) => new Promise((r) => setTimeout(r, ms));

  const data = {
    version: "0.1.0",
    data_dir: "C:\\Users\\you\\AppData\\Local\\ThineCraft\\DriveCraft\\data",
    setup: view === "setup" ? null : { client_id: "a8a192b4-0000-0000-0000-000000000000", tenant: "organizations" },
    signed_in: view !== "setup" && view !== "signin",
    can_write: true,
    is_windows: true,
    long_paths: false,
    auto_sync: false,
    autostart: false,
    link: { chosen: true, address: "192.168.1.9", name: "Ethernet 3" },
    links: [{ name: "Ethernet 2", address: "192.168.1.10" }, { name: "Ethernet 3", address: "192.168.1.9" }, { name: "Tailscale", address: "100.107.75.91" }],
    connections: view === "empty" ? [] : [
      { name: "Documents", mode: "ingest", folder: "C:\\Users\\shawn\\OneDrive\\Documents", folder_exists: true, library_url: "https://shawnklewis.sharepoint.com/Shared%20Documents", remote_files: 1871, remote_folders: 209, remote_bytes: 83990000, local_items: 1864, pending: 0, first_scan: "complete", last_sync: now - 120, syncing: false },
      { name: "TTSEC", mode: "sync", folder: "C:\\Work\\TTSEC", folder_exists: true, library_url: "https://contoso.sharepoint.com/sites/TTSEC/Shared%20Documents", remote_files: 184302, remote_folders: 9120, remote_bytes: 412_000_000_000, local_items: 193422, pending: 0, first_scan: "complete", last_sync: now - 240, syncing: false },
      { name: "Board Archive", mode: "archive", folder: "D:\\Archive\\Board", folder_exists: true, library_url: "https://contoso.sharepoint.com/sites/Board/Papers", remote_files: 5210, remote_folders: 310, remote_bytes: 18_400_000_000, local_items: 5520, pending: 0, first_scan: "complete", last_sync: now - 7200, syncing: false },
      { name: "File server migration", mode: "ingest", folder: "\\\\fileserver\\projects", folder_exists: true, library_url: "https://contoso.sharepoint.com/sites/Projects/Documents", remote_files: 0, remote_folders: 0, remote_bytes: 0, local_items: 0, pending: 0, first_scan: "not started", last_sync: null, syncing: false },
    ],
  };
  const problems = {
    TTSEC: [
      { kind: "failed", level: "bad", lasting: false, path: "Finance/FY27/Budget model.xlsx", message: "the file is open in another program", first_seen: now - 5400, last_seen: now - 240 },
      { kind: "copy_kept", level: "warn", lasting: true, path: "Policies/Travel.docx", message: "Different here and in the library. The library's version is in its place; yours was kept as \"Travel (local copy).docx\".", first_seen: now - 86400, last_seen: now - 86400 },
      { kind: "not_uploaded", level: "warn", lasting: false, path: "Contracts/Supplier: draft?.pdf", message: "Not uploaded: the name contains a character the server does not allow.", first_seen: now - 7200, last_seen: now - 240 },
      { kind: "long_path", level: "notice", lasting: true, path: "Projects/2026/Port Authority/Environmental impact assessment/Appendices/Appendix C - Marine survey raw data/Station 14/readings-final-v3.csv", message: "Its full path on this computer is longer than 260 characters. It is synchronized, but some programs will not open it.", first_seen: now - 172800, last_seen: now - 172800 },
    ],
  };
  const checkouts = {
    TTSEC: [
      { item_id: "1", path: "Finance/FY27/Budget model.xlsx", by: "Shawn Lewis", mine: true, since: now - 3600 },
      { item_id: "2", path: "Policies/Travel.docx", by: "Anita Ali", mine: false, since: now - 90000 },
    ],
  };
  const tallyOf = (name) => {
    const t = { bad: 0, warn: 0, notice: 0 };
    for (const p of problems[name] || []) t[p.level]++;
    return t;
  };
  const who = { name: "Shawn Lewis", account: "shawn@contoso.com", can_write: true };

  function report(extra) {
    return Object.assign({ scanned: 0, downloaded: 0, uploaded: 0, adopted: 0, unchanged: 0, folders_created: 0, remote_folders_created: 0, renamed: 0, moved_remote: 0, deleted_local: 0, deleted_remote: 0, conflicts: 0, skipped: 0, failed: 0, held_deletes: 0, long_paths: 0, bytes_down: 0, bytes_up: 0, warnings: [] }, extra);
  }

  const handlers = {
    async get_state() {
      const s = JSON.parse(JSON.stringify(data));
      for (const c of s.connections) {
        c.problems = tallyOf(c.name);
        const co = checkouts[c.name] || [];
        c.checked_out = { total: co.length, mine: co.filter((f) => f.mine).length };
      }
      return s;
    },
    async list_problems({ name }) { return JSON.parse(JSON.stringify(problems[name] || [])); },
    async dismiss_problems({ name, kind, path }) {
      problems[name] = (problems[name] || []).filter((p) => !p.lasting || (kind && !(p.kind === kind && p.path === path)));
    },
    async show_problem() {},
    async bin_page({ next }) {
      await wait(400);
      const lib = "sites/TTSEC/Shared Documents";
      const people = ["Shawn Lewis", "Anita Ali", "Kwasi Lake", "Gillian Lynch"];
      const names = ["Q3 forecast.xlsx", "Board minutes.docx", "Supplier agreement.pdf", "Site photo.jpg", "Old contracts", "Budget model.xlsx", "Travel policy.docx", "Launch deck.pptx", "Archive.zip", "Notes.txt"];
      const folders = ["", "/Finance/FY27", "/Policies", "/Contracts/2026", "/Board"];
      const start = Number(next || 0);
      const items = [];
      for (let i = start; i < Math.min(start + 400, 1200); i++) {
        items.push({ id: `b${i}`, name: i < 10 ? names[i] : `${names[i % 10].replace(/(\.\w+)?$/, ` ${i}$1`)}`, from: i % 17 === 3 ? "sites/TTSEC/Site Assets" : lib + folders[i % 5],
          size: 2000 + ((i * 7919) % 9000000), deleted_at: new Date(Date.now() - i * 5400e3).toISOString(), deleted_by: people[(i * 3) % 4] });
      }
      return { library_path: lib, items, next: start + 400 < 1200 ? String(start + 400) : null };
    },
    async restore_bin() { await wait(500); },
    async list_checkouts({ name }) { return JSON.parse(JSON.stringify(checkouts[name] || [])); },
    async check_file({ name, path }) { await wait(400); checkouts[name] = (checkouts[name] || []).filter((f) => f.path !== path); },
    async set_explorer({ name, enabled, label }) {
      await wait(600);
      const c = data.connections.find((x) => x.name === name);
      c.explorer = enabled ? { folder: c.folder, name: label || name, icon: "" } : null;
      return { done: enabled ? c.local_items : 0, failed: 0 };
    },
    async pick_icon() { return "C:\\Icons\\team.ico"; },
    async whoami() { return data.signed_in ? Object.assign({}, who, { can_write: data.can_write }) : null; },
    async save_setup({ clientId, tenant }) { data.setup = { client_id: clientId, tenant: tenant || "organizations" }; data.signed_in = false; },
    async login({ readOnly }) { await wait(900); data.signed_in = true; data.can_write = !readOnly; return Object.assign({}, who, { can_write: data.can_write }); },
    async logout() { data.signed_in = false; },
    async pick_folder() { return "C:\\Work\\Finance"; },
    async check_library({ address }) {
      await wait(400);
      if (!address.startsWith("https://") && address !== "onedrive") throw "not a web address: the library address must start with https://";
      return { name: "Documents", url: "https://contoso.sharepoint.com/sites/Finance/Shared%20Documents" };
    },
    async connect({ folder, mode, name }) {
      await wait(500);
      const n = name || folder.split(/[\\/]/).filter(Boolean).pop();
      data.connections.push({ name: n, mode, folder, folder_exists: false, library_url: "https://contoso.sharepoint.com/sites/Finance/Shared%20Documents", remote_files: 0, remote_folders: 0, remote_bytes: 0, local_items: 0, pending: 0, first_scan: "not started", last_sync: null, syncing: false });
      return n;
    },
    async set_mode({ name, mode }) { data.connections.find((c) => c.name === name).mode = mode; },
    async disconnect({ name }) { data.connections = data.connections.filter((c) => c.name !== name); },
    async open_connection() {},
    async open_logs() {},
    async publish_seed() { await wait(700); return { items: 193422, bytes: 9400000, name: "seed-1.sqlite.gz", replaced: 1 }; },
    async set_network({ address }) {
      const l = data.links.find((x) => x.address === address);
      data.link = l ? { chosen: true, address: l.address, name: l.name } : { chosen: false, address: "192.168.1.10", name: "Ethernet 2" };
    },
    async set_auto_sync({ enabled }) { data.auto_sync = enabled; },
    async set_autostart({ enabled }) { data.autostart = enabled; },
    async sync_everything() {},
    async sync({ name, dryRun, confirmDeletes }) {
      const c = data.connections.find((x) => x.name === name);
      emit("sync-started", name);
      const pre = dryRun ? "would " : "";
      const lines = [
        `${pre}download Board/2026-11 minutes.docx (48211 bytes)`,
        `${pre}upload Budget/FY27 draft.xlsx (912004 bytes, changed here)`,
        `${pre}keep a local copy of Policies/Travel.docx (changed here and on the server)`,
        "warning: Policies/Travel.docx differs from the server's copy; your version was kept as Travel (local copy).docx",
        `${pre}move Contracts/old name.pdf -> Contracts/Supplier agreement.pdf on the server`,
      ];
      for (const line of lines) { await wait(250); emit("sync-log", { name, line }); }
      let rep;
      if (c.mode !== "archive" && !data.can_write && !dryRun) {
        const f = { name, dry_run: dryRun, report: null, error: "You are signed in read-only, which cannot synchronize both ways. Sign in again with write access.", throttled: 0 };
        emit("sync-finished", f);
        return f;
      }
      rep = report({ downloaded: 1, uploaded: 1, moved_remote: 1, conflicts: 1, held_deletes: confirmDeletes ? 0 : 34, deleted_remote: confirmDeletes ? 34 : 0, bytes_down: 48211, bytes_up: 912004,
        warnings: ["Policies/Travel.docx differs from the server's copy; your version was kept as Travel (local copy).docx"].concat(confirmDeletes ? [] : ["34 items are missing here (more than 20). They were NOT deleted on the server; that needs your confirmation."]) });
      if (!dryRun) c.last_sync = Math.floor(Date.now() / 1000);
      const f = { name, dry_run: dryRun, report: rep, error: null, throttled: 2, details: ["timing: reading the server's changes 2.1s, updating this computer 14.8s, sending local changes 3.0s", "network: 212 requests, 0 retried after errors, asked to slow down 2 times (waited 37.0s in total)", "memory: peak 61.4 MB"] };
      emit("sync-finished", f);
      return f;
    },
  };

  window.__DC_MOCK__ = {
    invoke(cmd, args) {
      if (!handlers[cmd]) return Promise.reject(`mock: unknown command ${cmd}`);
      return handlers[cmd](args || {});
    },
    listen(name, fn) {
      (listeners[name] = listeners[name] || []).push(fn);
      return Promise.resolve(() => {});
    },
  };
})();
