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
    version: "0.1.1",
    data_dir: "C:\\Users\\you\\AppData\\Local\\ThineCraft\\DriveCraft\\data",
    setup: view === "setup" ? null : { client_id: "a8a192b4-0000-0000-0000-000000000000", tenant: "organizations" },
    signed_in: view !== "setup" && view !== "signin",
    can_write: true,
    is_windows: true,
    long_paths: false,
    auto_sync: false,
    autostart: false,
    sync_button: { ours: false, other: "OneDrive" },
    link: { chosen: true, address: "10.0.0.12", name: "Office network" },
    links: [{ name: "Wi-Fi", address: "10.0.0.21" }, { name: "Office network", address: "10.0.0.12" }, { name: "Tailscale", address: "100.64.0.7" }],
    connections: view === "empty" ? [] : [
      { name: "Documents", mode: "ingest", folder: "C:\\Users\\shawn\\OneDrive\\Documents", folder_exists: true, library_url: "https://contoso.sharepoint.com/Shared%20Documents", remote_files: 1871, remote_folders: 209, remote_bytes: 83990000, local_items: 1864, pending: 0, first_scan: "complete", last_sync: now - 120, syncing: false },
      { name: "Northwind", mode: "sync", folder: "C:\\Work\\Northwind", folder_exists: true, library_url: "https://contoso.sharepoint.com/sites/Northwind/Shared%20Documents", remote_files: 184302, remote_folders: 9120, remote_bytes: 412_000_000_000, local_items: 193422, pending: 0, first_scan: "complete", last_sync: now - 240, syncing: false },
      { name: "Board Archive", mode: "archive", folder: "D:\\Archive\\Board", folder_exists: true, library_url: "https://contoso.sharepoint.com/sites/Board/Papers", remote_files: 5210, remote_folders: 310, remote_bytes: 18_400_000_000, local_items: 5520, pending: 0, first_scan: "complete", last_sync: now - 7200, syncing: false },
      { name: "File server migration", mode: "ingest", folder: "\\\\fileserver\\projects", folder_exists: true, library_url: "https://contoso.sharepoint.com/sites/Projects/Documents", remote_files: 0, remote_folders: 0, remote_bytes: 0, local_items: 0, pending: 0, first_scan: "not started", last_sync: null, syncing: false },
    ],
  };
  const problems = {
    Northwind: [
      { kind: "failed", level: "bad", lasting: false, path: "Finance/FY27/Budget model.xlsx", message: "the file is open in another program", first_seen: now - 5400, last_seen: now - 240 },
      { kind: "copy_kept", level: "warn", lasting: true, path: "Policies/Travel.docx", message: "Different here and in the library. The library's version is in its place; yours was kept as \"Travel (local copy).docx\".", first_seen: now - 86400, last_seen: now - 86400 },
      { kind: "not_uploaded", level: "warn", lasting: false, path: "Contracts/Supplier: draft?.pdf", message: "Not uploaded: the name contains a character the server does not allow.", first_seen: now - 7200, last_seen: now - 240 },
      { kind: "long_path", level: "notice", lasting: true, path: "Projects/2026/Port Authority/Environmental impact assessment/Appendices/Appendix C - Marine survey raw data/Station 14/readings-final-v3.csv", message: "Its full path on this computer is longer than 260 characters. It is synchronized, but some programs will not open it.", first_seen: now - 172800, last_seen: now - 172800 },
    ],
  };
  const col = (name, title, kind, value, extra) => Object.assign({ name, title, kind, editable: true, required: false, choices: [], multi: false, date_only: false, description: "", value }, extra || {});
  const mockDetails = { connection: "Northwind", path: "Finance/FY27/Budget model.xlsx", modified_by: "Priya Nair", created_by: "Alex Morgan", columns: [
    col("Title", "Title", "text", "FY27 budget model"),
    col("Department", "Department", "choice", "Finance", { choices: ["Finance", "Legal", "Operations"], required: true }),
    col("Status", "Approval status", "choice", "Draft", { choices: ["Draft", "In review", "Approved"] }),
    col("Regions", "Regions", "choice", ["North"], { choices: ["North", "South", "East", "West"], multi: true }),
    col("Due", "Review due", "date", "2026-11-30T04:00:00Z", { date_only: true }),
    col("Pages", "Pages", "number", 42),
    col("Final", "Final version", "yes_no", false),
    col("Notes", "Notes", "note", "Waiting on Q3 actuals.", { description: "Anything reviewers should know" }),
    col("Owner", "Owner", "other", null, { editable: false }),
    col("_UIVersionString", "Version", "text", "12.0", { editable: false }),
  ] };
  const checkouts = {
    Northwind: [
      { item_id: "1", path: "Finance/FY27/Budget model.xlsx", by: "Alex Morgan", mine: true, since: now - 3600 },
      { item_id: "2", path: "Policies/Travel.docx", by: "Priya Nair", mine: false, since: now - 90000 },
    ],
  };
  const tallyOf = (name) => {
    const t = { bad: 0, warn: 0, notice: 0 };
    for (const p of problems[name] || []) t[p.level]++;
    return t;
  };
  const who = { name: "Alex Morgan", account: "alex@contoso.com", can_write: true };

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
    async open_site() {},  // also takes { page: "setup" }
    async pick_library_file() { return "C:\\Work\\Northwind\\Finance\\FY27\\Budget model.xlsx"; },
    async take_details_request() { return null; },
    async open_in_library() {},
    async file_details({ file }) { await wait(400); return JSON.parse(JSON.stringify(Object.assign({ file }, mockDetails))); },
    async save_details({ file, changes }) {
      await wait(500);
      for (const c of mockDetails.columns) if (c.name in changes) c.value = c.kind === "yes_no" ? changes[c.name] === "yes" : c.multi ? changes[c.name].split("; ").filter(Boolean) : changes[c.name] || null;
      return JSON.parse(JSON.stringify(Object.assign({ file }, mockDetails)));
    },
    async set_sync_button({ enabled }) { data.sync_button = { ours: enabled, other: "OneDrive" }; return data.sync_button; },
    async take_sync_link() {
      if (new URLSearchParams(location.search).get("sync") === null || data.linkTaken) return null;
      data.linkTaken = true;
      return { url: "odopen://sync/?x=1", site_title: "Finance Team", library_title: "Documents", address: "https://contoso.sharepoint.com/sites/Finance/Shared%20Documents", suggested_name: "Finance Team - Documents", suggested_folder: "C:\\Users\\you\\DriveCraft\\Finance Team - Documents", existing: null, other_account: null, folder: null, other: "OneDrive" };
    },
    async pass_on_sync_link() {},
    async bin_page({ next }) {
      await wait(400);
      const lib = "sites/Northwind/Shared Documents";
      const people = ["Alex Morgan", "Priya Nair", "Tom Okafor", "Maria Santos"];
      const names = ["Q3 forecast.xlsx", "Board minutes.docx", "Supplier agreement.pdf", "Site photo.jpg", "Old contracts", "Budget model.xlsx", "Travel policy.docx", "Launch deck.pptx", "Archive.zip", "Notes.txt"];
      const folders = ["", "/Finance/FY27", "/Policies", "/Contracts/2026", "/Board"];
      const start = Number(next || 0);
      const items = [];
      for (let i = start; i < Math.min(start + 400, 1200); i++) {
        items.push({ id: `b${i}`, name: i < 10 ? names[i] : `${names[i % 10].replace(/(\.\w+)?$/, ` ${i}$1`)}`, from: i % 17 === 3 ? "sites/Northwind/Site Assets" : lib + folders[i % 5],
          size: 2000 + ((i * 7919) % 9000000), deleted_at: new Date(Date.now() - i * 5400e3).toISOString(), deleted_by: people[(i * 3) % 4] });
      }
      return { library_path: lib, items, next: start + 400 < 1200 ? String(start + 400) : null };
    },
    async restore_bin() { await wait(500); },
    async library_permissions({ name, refresh }) {
      await wait(refresh ? 900 : 150);
      // "Board Archive" shows what someone who is not an owner sees.
      const full = name !== "Board Archive";
      const p = (kind, n, roles, extra) => Object.assign({ perm_id: n, kind, principal_id: kind === "link" ? null : n.toLowerCase().replace(/\W+/g, "-"), name: n, email: null, login: null, roles, link_scope: null, via_link: false }, extra || {});
      const person = (n, roles, extra) => p("user", n, roles, Object.assign({ email: `${n.split(" ")[0].toLowerCase()}@contoso.com` }, extra || {}));
      const owners = p("site_group", `${name} Owners`, ["owner"]);
      const members = p("site_group", `${name} Members`, ["write"]);
      const visitors = p("site_group", `${name} Visitors`, ["read"]);
      const entries = [
        { id: "root", path: "", folder: true, nesting: 0, state: "read", permissions: full ? [owners, members, visitors] : [members] },
        { id: "a", path: "Board", folder: true, nesting: 1, state: "read", permissions: [owners, person("Maria Santos", ["owner"]), p("group", "Board of Directors", ["read"])] },
        { id: "b", path: "Board/2026/Confidential minutes", folder: true, nesting: 2, state: "read", permissions: [owners, person("Maria Santos", ["owner"])] },
        { id: "c", path: "Finance", folder: true, nesting: 1, state: "read", permissions: [owners, p("group", "Finance Team", ["write"]), person("Alex Morgan", ["read"])] },
        { id: "d", path: "Finance/FY27/Budget model.xlsx", folder: false, nesting: 2, state: "read", permissions: [owners, p("group", "Finance Team", ["write"]), p("link", "view link (organization)", ["read"], { link_scope: "organization" })] },
        { id: "e", path: "HR", folder: true, nesting: 1, state: "read", permissions: [owners, person("Dana Whitfield", ["owner"]), p("group", "HR Team", ["write"])] },
        { id: "f", path: "HR/Reviews/2026 performance reviews for the Northwind Traders regional sales and customer service teams", folder: true, nesting: 2, state: "read", permissions: [person("Dana Whitfield", ["owner"])] },
        { id: "g", path: "Legal/Supplier agreements", folder: true, nesting: 1, state: "refused", permissions: [] },
        { id: "h", path: "Marketing/Launch plan.pptx", folder: false, nesting: 1, state: "read", permissions: [members, p("link", "edit link (users)", ["write"], { link_scope: "users" }), person("Tom Okafor", ["write"], { via_link: true, link_scope: "users" }), person("Priya Nair", ["write"], { via_link: true, link_scope: "users" })] },
      ];
      return { read_at: now - (refresh ? 0 : 3 * 3600), full, entries: full ? entries : entries.slice(0, 1).concat(entries.slice(3, 5)), refused: refresh ? 1 : 0 };
    },
    async list_checkouts({ name }) { return JSON.parse(JSON.stringify(checkouts[name] || [])); },
    async check_file({ name, path }) { await wait(400); checkouts[name] = (checkouts[name] || []).filter((f) => f.path !== path); },
    async set_explorer({ name, enabled, label }) {
      await wait(600);
      const c = data.connections.find((x) => x.name === name);
      c.explorer = enabled ? { folder: c.folder, name: label || name, icon: "" } : null;
      return { done: enabled ? c.local_items : 0, failed: 0 };
    },
    async save_icon({ key }) { return `C:\\Users\\you\\AppData\\Local\\ThineCraft\\DriveCraft\\data\\icons\\${key}.ico`; },
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
    async publish_seed() { await wait(700); return { items: 193422, withheld: 3, bytes: 9400000, name: "seed-1.sqlite.gz", replaced: 1 }; },
    async set_network({ address }) {
      const l = data.links.find((x) => x.address === address);
      data.link = l ? { chosen: true, address: l.address, name: l.name } : { chosen: false, address: "10.0.0.21", name: "Wi-Fi" };
    },
    async set_auto_sync({ enabled }) { data.auto_sync = enabled; },
    async set_autostart({ enabled }) { data.autostart = enabled; },
    async sync_everything() {},
    async sync({ name, dryRun, confirmDeletes }) {
      const c = data.connections.find((x) => x.name === name);
      emit("sync-started", name);
      const pre = dryRun ? "would " : "";
      const lines = [
        "stage: Asking the library what has changed",
        "stage: Reading the library: 5,039 items so far",
        "stage: Comparing the library with this computer: 212 of 4,880 changed items",
        "stage: Looking for changes on this computer: 18,204 items checked (now in Finance/FY27)",
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
