// File sharing UI: tabs, identity + contacts, transfers, offers, settings.
// Talks to src-tauri/src/share.rs (commands + `share://*` events).
// Loaded as its own module so it keeps working independently of main.ts.

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import qrcode from "qrcode-generator";

// ─── Types (mirror share.rs / wzp-share JSON) ────────────────────────

type Presence = "online" | "offline" | "unknown";
type TState =
  | "connecting"
  | "awaiting_decision"
  | "transferring"
  | "completed"
  | "rejected"
  | "failed"
  | "cancelled";

interface Transfer {
  id: string;
  contact: string;
  contact_name: string;
  direction: "outgoing" | "incoming";
  name: string;
  size: number;
  path: string | null;
  state: TState;
  done: number;
  relayed: boolean | null;
  sas: number | null;
  error: string | null;
  saved_to?: string | null;
}

interface Contact {
  id: string;
  name: string;
  fingerprint: string;
  verified: boolean;
  presence: Presence;
  last_seen: number | null;
  added_at: number;
}

interface ContactRequest {
  id: string;
  fingerprint: string;
  name: string | null;
  contact: string | null;
}

interface Identity {
  id: string;
  fingerprint: string;
  contact: string;
  display_name: string;
  platform: string;
}

type IrohRelay = { mode: "n0" } | { mode: "custom"; url: string } | { mode: "off" };

interface SettingsView {
  display_name: string;
  effective_display_name: string;
  relay_addr: string;
  relay_token_set: boolean;
  iroh_relay: IrohRelay;
  download_dir: string;
  download_dir_is_default: boolean;
  auto_accept_verified: boolean;
  platform: string;
}

interface NodeStatus {
  running: boolean;
  starting: boolean;
  error: string | null;
  warning: string | null;
}

interface PickedFile {
  path: string;
  name: string;
  size: number;
  mime: string | null;
}

interface SendResult {
  path: string;
  id: string | null;
  error: string | null;
}

type ShareEvent =
  | { type: "presence"; id: string; state: Presence; last_seen: number | null }
  | { type: "contact_request"; id: string; fingerprint: string; name: string | null; contact: string | null }
  | { type: "text"; from: string; text: string; sent_at_ms: number }
  | { type: "transfer_progress"; id: string; done: number; total: number }
  | { type: "transfer_cancelled"; transfer: Transfer; by_peer: boolean }
  | { type: "invite_refused"; from: string; reason: string }
  | {
      type:
        | "transfer_offered"
        | "transfer_started"
        | "transfer_completed"
        | "transfer_rejected"
        | "transfer_failed";
      transfer: Transfer;
    };

// ─── State ───────────────────────────────────────────────────────────

const transfers = new Map<string, Transfer>();
const speed = new Map<string, { t: number; done: number; bps: number }>();
let contacts: Contact[] = [];
let requests: ContactRequest[] = [];
let identity: Identity | null = null;
let settings: SettingsView | null = null;
let status: NodeStatus = { running: false, starting: true, error: null, warning: null };
let pendingShared: PickedFile[] = [];
const isAndroid = /android/i.test(navigator.userAgent);

// ─── Helpers ─────────────────────────────────────────────────────────

const $ = <T extends HTMLElement = HTMLElement>(id: string) => document.getElementById(id) as T;

type Child = Node | string | null | undefined | false;
function h<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  attrs: Record<string, string | boolean | ((e: Event) => void)> = {},
  ...children: Child[]
): HTMLElementTagNameMap[K] {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (typeof v === "function") el.addEventListener(k.replace(/^on/, ""), v);
    else if (v === true) el.setAttribute(k, "");
    else if (v !== false) el.setAttribute(k, v);
  }
  for (const c of children) {
    if (c === null || c === undefined || c === false) continue;
    el.append(typeof c === "string" ? document.createTextNode(c) : c);
  }
  return el;
}

function fmtSize(n: number): string {
  const u = ["B", "KB", "MB", "GB", "TB"];
  let v = n;
  let i = 0;
  while (v >= 1024 && i < u.length - 1) {
    v /= 1024;
    i++;
  }
  return i === 0 ? `${n} B` : `${v.toFixed(v < 10 ? 1 : 0)} ${u[i]}`;
}

function fmtDuration(s: number): string {
  if (!isFinite(s) || s < 0) return "";
  if (s < 60) return `${Math.ceil(s)} s`;
  if (s < 3600) return `${Math.floor(s / 60)} min ${Math.round(s % 60)} s`;
  return `${Math.floor(s / 3600)} h ${Math.round((s % 3600) / 60)} min`;
}

function errText(e: unknown): string {
  return typeof e === "string" ? e : e instanceof Error ? e.message : JSON.stringify(e);
}

let toastTimer = 0;
function toast(msg: string, kind: "info" | "error" = "info") {
  const el = $("fs-toast");
  el.textContent = msg;
  el.className = `fs-toast ${kind}`;
  clearTimeout(toastTimer);
  toastTimer = window.setTimeout(() => el.classList.add("hidden"), kind === "error" ? 6000 : 3000);
}

async function copyText(text: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    const ta = h("textarea", {}, text);
    document.body.append(ta);
    ta.select();
    const ok = document.execCommand("copy");
    ta.remove();
    return ok;
  }
}

function contactName(id: string): string {
  return contacts.find(c => c.id === id)?.name || id.slice(0, 8);
}

// ─── Modal ───────────────────────────────────────────────────────────

let modalCloseCb: (() => void) | null = null;
let modalReturnFocus: HTMLElement | null = null;
let modalKey: string | null = null;

interface ModalAction {
  label: string;
  kind?: "primary" | "secondary" | "danger";
  run: () => void | boolean | Promise<void | boolean>; // return false to keep open
}

function openModal(title: string, body: Child[], actions: ModalAction[], key: string | null = null, onClose?: () => void) {
  if (!$("fs-modal").classList.contains("hidden")) closeModal();
  modalReturnFocus = document.activeElement as HTMLElement | null;
  modalKey = key;
  modalCloseCb = onClose ?? null;
  $("fs-modal-title").textContent = title;
  $("fs-modal-body").replaceChildren(...(body.filter(Boolean) as (Node | string)[]));
  const acts = $("fs-modal-actions");
  acts.replaceChildren(
    ...actions.map(a =>
      h(
        "button",
        {
          type: "button",
          class: a.kind === "primary" ? "fs-primary" : a.kind === "danger" ? "fs-danger" : "fs-secondary",
          onclick: async (e: Event) => {
            const btn = e.currentTarget as HTMLButtonElement;
            btn.disabled = true;
            try {
              const r = await a.run();
              if (r !== false) closeModal(false);
            } catch (err) {
              toast(errText(err), "error");
            } finally {
              btn.disabled = false;
            }
          },
        },
        a.label,
      ),
    ),
  );
  $("fs-modal").classList.remove("hidden");
  const first = acts.querySelector<HTMLButtonElement>("button.fs-primary") ?? acts.querySelector("button");
  first?.focus();
}

function closeModal(runCb = true) {
  if ($("fs-modal").classList.contains("hidden")) return;
  $("fs-modal").classList.add("hidden");
  const cb = modalCloseCb;
  modalCloseCb = null;
  modalKey = null;
  if (runCb && cb) cb();
  modalReturnFocus?.focus?.();
}

$("fs-modal").addEventListener("click", e => {
  if (e.target === $("fs-modal")) closeModal();
});
document.addEventListener("keydown", e => {
  if (e.key === "Escape" && !$("fs-modal").classList.contains("hidden")) closeModal();
});

// ─── Tabs ────────────────────────────────────────────────────────────

const TABS = ["files", "contacts", "calls", "settings"] as const;
type Tab = (typeof TABS)[number];

function showTab(tab: Tab) {
  document.body.dataset.tab = tab;
  for (const t of TABS) {
    const btn = $(`tab-${t}`);
    const on = t === tab;
    btn.setAttribute("aria-selected", String(on));
    btn.tabIndex = on ? 0 : -1;
    $(`view-${t}`).classList.toggle("hidden", on ? false : t !== "calls");
  }
  // The calls view stays in the DOM (its call drawer / incoming-call banner
  // are fixed overlays that must stay visible); CSS hides the rest of it.
  $("view-calls").classList.toggle("view-inactive", tab !== "calls");
  if (tab === "contacts") void refreshContacts();
  if (tab === "settings") void loadSettings();
  window.scrollTo(0, 0);
}

for (const t of TABS) $(`tab-${t}`).addEventListener("click", () => showTab(t));
$("tabbar").addEventListener("keydown", e => {
  const i = TABS.indexOf(document.body.dataset.tab as Tab);
  let n = -1;
  if (e.key === "ArrowRight") n = (i + 1) % TABS.length;
  if (e.key === "ArrowLeft") n = (i + TABS.length - 1) % TABS.length;
  if (n >= 0) {
    e.preventDefault();
    showTab(TABS[n]);
    $(`tab-${TABS[n]}`).focus();
  }
});
$("fs-open-call-settings").addEventListener("click", () => $("settings-btn").click());

// Incoming call while on another tab: jump to Calls.
const callBanner = document.getElementById("incoming-call-banner");
if (callBanner) {
  new MutationObserver(() => {
    if (!callBanner.classList.contains("hidden") && document.body.dataset.tab !== "calls") showTab("calls");
  }).observe(callBanner, { attributes: true, attributeFilter: ["class"] });
}

// ─── Status ──────────────────────────────────────────────────────────

function renderStatus() {
  const el = $("fs-status");
  el.className = "fs-status";
  if (status.error) {
    el.textContent = `File sharing is not running: ${status.error}`;
    el.classList.add("error");
  } else if (status.starting) {
    el.textContent = "Starting…";
  } else if (status.warning) {
    el.textContent = status.warning;
    el.classList.add("warn");
  } else {
    const n = contacts.filter(c => c.presence === "online").length;
    el.textContent = `Ready · ${n} of ${contacts.length} contacts online`;
    el.classList.add("ok");
  }
}

// ─── Identity & contacts ─────────────────────────────────────────────

async function refreshIdentity() {
  try {
    identity = await invoke<Identity>("get_share_identity");
  } catch {
    return;
  }
  $("fs-my-name").textContent = identity.display_name;
  $("fs-my-fp").textContent = identity.fingerprint;
  ($("fs-my-contact") as HTMLTextAreaElement).value = identity.contact;
  try {
    // Upper case → QR alphanumeric mode (A–Z, 2–7), smaller code.
    const qr = qrcode(0, "M");
    qr.addData(identity.contact.toUpperCase(), "Alphanumeric");
    qr.make();
    $("fs-qr").innerHTML = qr.createSvgTag({ cellSize: 4, margin: 2, scalable: true });
  } catch {
    $("fs-qr").textContent = "";
  }
}

$("fs-copy-contact").addEventListener("click", async () => {
  if (!identity) return;
  toast((await copyText(identity.contact)) ? "Contact string copied" : "Copy failed", "info");
});

async function refreshContacts() {
  try {
    contacts = await invoke<Contact[]>("list_contacts");
    requests = await invoke<ContactRequest[]>("list_contact_requests");
  } catch {
    return;
  }
  renderContacts();
  renderRequests();
  renderStatus();
}

function presenceLabel(p: Presence) {
  return p === "online" ? "online" : p === "offline" ? "offline" : "status unknown";
}

function renderContacts() {
  const ul = $("fs-contacts");
  $("fs-contacts-empty").classList.toggle("hidden", contacts.length > 0);
  ul.replaceChildren(
    ...contacts.map(c =>
      h(
        "li",
        { class: "fs-item fs-contact", "data-contact": c.id },
        h("span", { class: `fs-dot ${c.presence}`, title: presenceLabel(c.presence) }),
        h(
          "div",
          { class: "fs-item-main" },
          h(
            "div",
            { class: "fs-item-title" },
            c.name || "(no name)",
            c.verified ? h("span", { class: "fs-badge ok", title: "Fingerprint verified" }, "✓ verified") : null,
          ),
          h("div", { class: "fs-item-sub" }, `${presenceLabel(c.presence)} · `, h("code", {}, c.fingerprint.slice(0, 19) + "…")),
        ),
        h(
          "div",
          { class: "fs-item-actions" },
          h("button", { type: "button", class: "fs-primary small", onclick: () => chooseAndSend(c.id), "aria-label": `Send files to ${c.name}` }, "Send"),
          h("button", { type: "button", class: "fs-secondary small", onclick: () => contactMenu(c), "aria-label": `More for ${c.name}` }, "⋯"),
        ),
      ),
    ),
  );
}

function verifyDialog(c: Contact) {
  openModal(
    `Verify ${c.name}`,
    [
      h("p", {}, "Compare this fingerprint with the one shown on their device (in person or on a call). Only mark the contact verified if every group matches."),
      h("code", { class: "fs-fp big" }, c.fingerprint),
    ],
    [
      { label: "Cancel", run: () => {} },
      {
        label: "It matches — mark verified",
        kind: "primary",
        run: async () => {
          await invoke("verify_contact", { id: c.id });
          toast(`${c.name} is verified`);
          await refreshContacts();
        },
      },
    ],
  );
}

function contactMenu(c: Contact) {
  openModal(
    c.name || "Contact",
    [
      h("div", { class: "fs-label" }, "Fingerprint"),
      h("code", { class: "fs-fp big" }, c.fingerprint),
      h("p", { class: "fs-hint" }, c.verified ? "Verified." : "Not verified yet (trusted on first use)."),
    ],
    [
      { label: "Close", run: () => {} },
      ...(c.verified ? [] : [{ label: "Verify…", kind: "secondary" as const, run: () => { setTimeout(() => verifyDialog(c)); } }]),
      {
        label: "Remove",
        kind: "danger",
        run: () => {
          setTimeout(() =>
            openModal(`Remove ${c.name}?`, [h("p", {}, "They will no longer be able to send you files until you add them again.")], [
              { label: "Cancel", run: () => {} },
              {
                label: "Remove",
                kind: "danger",
                run: async () => {
                  await invoke("remove_contact", { id: c.id });
                  await refreshContacts();
                },
              },
            ]),
          );
        },
      },
    ],
  );
}

function renderRequests() {
  $("fs-requests-wrap").classList.toggle("hidden", requests.length === 0);
  const badge = $("tab-contacts-badge");
  badge.textContent = String(requests.length);
  badge.classList.toggle("hidden", requests.length === 0);
  $("fs-requests").replaceChildren(
    ...requests.map(r =>
      h(
        "li",
        { class: "fs-item" },
        h(
          "div",
          { class: "fs-item-main" },
          h("div", { class: "fs-item-title" }, r.name || "Unknown peer"),
          h("div", { class: "fs-item-sub" }, h("code", {}, r.fingerprint)),
          r.contact ? null : h("div", { class: "fs-hint" }, "No contact string sent; adding works if they can be found by id."),
        ),
        h(
          "div",
          { class: "fs-item-actions" },
          h(
            "button",
            {
              type: "button",
              class: "fs-primary small",
              onclick: async () => {
                try {
                  const c = await invoke<Contact>("add_contact", { contact: r.contact || r.id });
                  toast(`Added ${c.name || "contact"}`);
                } catch (e) {
                  toast(errText(e), "error");
                }
                await refreshContacts();
              },
            },
            "Add",
          ),
          h(
            "button",
            {
              type: "button",
              class: "fs-secondary small",
              onclick: async () => {
                await invoke("dismiss_contact_request", { id: r.id });
                await refreshContacts();
              },
            },
            "Dismiss",
          ),
        ),
      ),
    ),
  );
}

$("fs-add-btn").addEventListener("click", async () => {
  const input = $("fs-add-input") as HTMLTextAreaElement;
  const msg = $("fs-add-msg");
  const v = input.value.trim();
  if (!v) {
    msg.textContent = "Paste a contact string first.";
    msg.className = "fs-msg error";
    return;
  }
  if (identity && v.toLowerCase() === identity.contact.toLowerCase()) {
    msg.textContent = "That is your own contact string.";
    msg.className = "fs-msg error";
    return;
  }
  try {
    const c = await invoke<Contact>("add_contact", { contact: v });
    input.value = "";
    msg.textContent = `Added ${c.name || "contact"}. Compare fingerprints to verify them.`;
    msg.className = "fs-msg ok";
    await refreshContacts();
  } catch (e) {
    msg.textContent = errText(e);
    msg.className = "fs-msg error";
  }
});

// ─── Sending ─────────────────────────────────────────────────────────

async function sendPaths(contact: string, paths: string[]) {
  if (paths.length === 0) return;
  try {
    const res = await invoke<SendResult[]>("send_files", { contact, paths });
    const failed = res.filter(r => r.error);
    if (failed.length) toast(failed.map(f => `${f.path.split(/[\\/]/).pop()}: ${f.error}`).join("\n"), "error");
    else toast(`Sending ${res.length} file${res.length > 1 ? "s" : ""} to ${contactName(contact)}`);
    showTab("files");
    await refreshTransfers();
  } catch (e) {
    toast(errText(e), "error");
  }
}

async function chooseAndSend(contact: string) {
  let files: PickedFile[];
  try {
    files = await invoke<PickedFile[]>("pick_files");
  } catch (e) {
    toast(errText(e), "error");
    return;
  }
  await sendPaths(contact, files.map(f => f.path));
}

function pickContact(title: string, onPick: (id: string) => void) {
  if (contacts.length === 0) {
    toast("Add a contact first (Contacts tab).", "error");
    showTab("contacts");
    return;
  }
  openModal(
    title,
    [
      h(
        "ul",
        { class: "fs-list fs-pick" },
        ...contacts.map(c =>
          h(
            "li",
            {},
            h(
              "button",
              { type: "button", class: "fs-pick-btn", onclick: () => { closeModal(false); onPick(c.id); } },
              h("span", { class: `fs-dot ${c.presence}`, title: presenceLabel(c.presence) }),
              h("span", {}, c.name || c.fingerprint),
              c.verified ? h("span", { class: "fs-badge ok" }, "✓") : null,
            ),
          ),
        ),
      ),
    ],
    [{ label: "Cancel", run: () => {} }],
  );
}

$("fs-send-btn").addEventListener("click", async () => {
  await refreshContacts();
  pickContact("Send files to…", id => void chooseAndSend(id));
});

// Android "Share to WarzonePhone": hold the files until a contact is chosen.
function renderShared() {
  const bar = $("fs-shared-bar");
  bar.classList.toggle("hidden", pendingShared.length === 0);
  if (!pendingShared.length) return;
  const total = pendingShared.reduce((a, f) => a + f.size, 0);
  bar.replaceChildren(
    h("div", { class: "fs-item-title" }, `${pendingShared.length} file${pendingShared.length > 1 ? "s" : ""} ready to send (${fmtSize(total)})`),
    h("div", { class: "fs-item-sub" }, pendingShared.map(f => f.name).join(", ")),
    h(
      "div",
      { class: "fs-row" },
      h(
        "button",
        {
          type: "button",
          class: "fs-primary small",
          onclick: () =>
            pickContact("Send shared files to…", async id => {
              const paths = pendingShared.map(f => f.path);
              pendingShared = [];
              renderShared();
              await sendPaths(id, paths);
            }),
        },
        "Choose contact",
      ),
      h("button", { type: "button", class: "fs-secondary small", onclick: () => { void invoke("discard_picked", { paths: pendingShared.map(f => f.path) }); pendingShared = []; renderShared(); } }, "Discard"),
    ),
  );
}

function addShared(files: PickedFile[]) {
  if (!files.length) return;
  pendingShared.push(...files);
  renderShared();
  showTab("files");
}

async function drainShared() {
  try {
    const files = await invoke<PickedFile[]>("take_shared_files");
    const known = new Set(pendingShared.map(f => f.path));
    addShared(files.filter(f => !known.has(f.path)));
  } catch {
    /* not Android / bridge unavailable */
  }
}

// Desktop drag-and-drop: drop files on a contact (Contacts tab) or anywhere
// in the Files tab (then pick a contact).
async function setupDragDrop() {
  if (isAndroid) return;
  try {
    const { getCurrentWebview } = await import("@tauri-apps/api/webview");
    let hover: Element | null = null;
    const target = (x: number, y: number) => {
      const r = window.devicePixelRatio || 1;
      return document.elementFromPoint(x / r, y / r)?.closest("[data-contact]") ?? null;
    };
    await getCurrentWebview().onDragDropEvent(ev => {
      const p = ev.payload;
      if (p.type === "over" || p.type === "enter") {
        const t = target(p.position.x, p.position.y);
        if (t !== hover) {
          hover?.classList.remove("drop-target");
          t?.classList.add("drop-target");
          hover = t;
        }
        document.body.classList.add("dragging");
      } else if (p.type === "leave") {
        hover?.classList.remove("drop-target");
        hover = null;
        document.body.classList.remove("dragging");
      } else if (p.type === "drop") {
        hover?.classList.remove("drop-target");
        hover = null;
        document.body.classList.remove("dragging");
        const t = target(p.position.x, p.position.y);
        const id = t?.getAttribute("data-contact");
        if (id) void sendPaths(id, p.paths);
        else if (p.paths.length) pickContact(`Send ${p.paths.length} file${p.paths.length > 1 ? "s" : ""} to…`, cid => void sendPaths(cid, p.paths));
      }
    });
  } catch {
    // Not in Tauri (vite dev in a browser) — ignore.
  }
}

// ─── Transfers ───────────────────────────────────────────────────────

const STATE_LABEL: Record<TState, string> = {
  connecting: "Connecting…",
  awaiting_decision: "Waiting for decision",
  transferring: "Transferring",
  completed: "Done",
  rejected: "Declined",
  failed: "Failed",
  cancelled: "Cancelled",
};

function isFinished(s: TState) {
  return s === "completed" || s === "rejected" || s === "failed" || s === "cancelled";
}

function upsertTransfer(t: Transfer) {
  const prev = transfers.get(t.id);
  transfers.set(t.id, { ...t, saved_to: t.saved_to ?? prev?.saved_to ?? null });
}

function trackSpeed(id: string, done: number) {
  const now = performance.now();
  const s = speed.get(id);
  if (!s) {
    speed.set(id, { t: now, done, bps: 0 });
    return;
  }
  const dt = (now - s.t) / 1000;
  if (dt < 0.5) return;
  const inst = Math.max(0, (done - s.done) / dt);
  s.bps = s.bps ? s.bps * 0.7 + inst * 0.3 : inst;
  s.t = now;
  s.done = done;
}

let renderQueued = false;
function scheduleRender() {
  if (renderQueued) return;
  renderQueued = true;
  requestAnimationFrame(() => {
    renderQueued = false;
    renderTransfers();
    renderOffers();
  });
}

function transferActions(t: Transfer): HTMLElement[] {
  const out: HTMLElement[] = [];
  const btn = (label: string, cls: string, fn: () => Promise<unknown>, aria?: string) =>
    h(
      "button",
      {
        type: "button",
        class: cls,
        "aria-label": aria ?? `${label} ${t.name}`,
        onclick: async () => {
          try {
            await fn();
          } catch (e) {
            toast(errText(e), "error");
          }
          await refreshTransfers();
        },
      },
      label,
    );
  if (t.direction === "incoming" && t.state === "awaiting_decision") {
    out.push(btn("Accept", "fs-primary small", () => invoke("accept", { id: t.id })));
    out.push(btn("Decline", "fs-secondary small", () => invoke("reject", { id: t.id, reason: "declined" })));
  } else if (!isFinished(t.state)) {
    out.push(btn("Cancel", "fs-secondary small", () => invoke("cancel", { id: t.id })));
  }
  if (t.state === "completed" && !isAndroid && t.path) {
    out.push(btn("Open", "fs-secondary small", () => invoke("open_file", { id: t.id })));
    out.push(btn("Show", "fs-secondary small", () => invoke("reveal_in_folder", { id: t.id }), `Show ${t.name} in folder`));
  }
  return out;
}

function renderTransfers() {
  const list = [...transfers.values()].reverse();
  $("fs-transfers-empty").classList.toggle("hidden", list.length > 0);
  // Active first, then finished; stable otherwise.
  list.sort((a, b) => Number(isFinished(a.state)) - Number(isFinished(b.state)));
  $("fs-transfers").replaceChildren(
    ...list.map(t => {
      const pct = t.size > 0 ? Math.min(100, (t.done / t.size) * 100) : t.state === "completed" ? 100 : 0;
      const sp = speed.get(t.id);
      let detail = `${t.direction === "outgoing" ? "To" : "From"} ${t.contact_name || contactName(t.contact)} · ${fmtSize(t.size)}`;
      if (t.state === "transferring") {
        detail = `${fmtSize(t.done)} of ${fmtSize(t.size)}`;
        if (sp && sp.bps > 0) detail += ` · ${fmtSize(sp.bps)}/s · ${fmtDuration((t.size - t.done) / sp.bps)} left`;
      }
      const badges: Child[] = [];
      if (t.relayed !== null) badges.push(h("span", { class: `fs-badge ${t.relayed ? "relay" : "direct"}` }, t.relayed ? "relayed" : "direct"));
      if (t.sas !== null && !isFinished(t.state)) badges.push(h("span", { class: "fs-badge", title: "Verification code — the other side sees the same" }, `code ${String(t.sas).padStart(4, "0")}`));
      return h(
        "li",
        { class: `fs-item fs-transfer ${t.state}` },
        h("span", { class: `fs-dir ${t.direction}`, "aria-label": t.direction === "outgoing" ? "sending" : "receiving" }, t.direction === "outgoing" ? "↑" : "↓"),
        h(
          "div",
          { class: "fs-item-main" },
          h("div", { class: "fs-item-title", title: t.name }, t.name || "(waiting for file details)"),
          h("div", { class: "fs-item-sub" }, h("span", { class: `fs-state ${t.state}` }, STATE_LABEL[t.state]), " · ", detail, ...badges.map(b => (b ? [" ", b] : [])).flat()),
          t.state === "transferring" || t.state === "connecting" || t.state === "awaiting_decision"
            ? h(
                "div",
                {
                  class: "fs-progress",
                  role: "progressbar",
                  "aria-valuemin": "0",
                  "aria-valuemax": "100",
                  "aria-valuenow": pct.toFixed(0),
                  "aria-label": `${t.name} progress`,
                },
                h("div", { class: "fs-progress-fill", style: `width:${pct.toFixed(1)}%` }),
              )
            : null,
          t.error && t.state !== "completed" ? h("div", { class: "fs-item-err" }, t.error) : null,
          t.saved_to ? h("div", { class: "fs-hint" }, `Saved to ${t.saved_to}`) : null,
        ),
        h("div", { class: "fs-item-actions" }, ...transferActions(t)),
      );
    }),
  );
  const pending = list.filter(t => t.direction === "incoming" && t.state === "awaiting_decision").length;
  const badge = $("tab-files-badge");
  badge.textContent = String(pending);
  badge.classList.toggle("hidden", pending === 0);
}

// Incoming offers: inline cards plus a dialog for the oldest undecided one.
const dismissedOffers = new Set<string>();
function renderOffers() {
  const offers = [...transfers.values()].filter(t => t.direction === "incoming" && t.state === "awaiting_decision");
  $("fs-offers").replaceChildren(
    ...offers.map(t =>
      h(
        "div",
        { class: "fs-card fs-offer" },
        h("div", { class: "fs-item-title" }, `${t.contact_name || contactName(t.contact)} wants to send you a file`),
        h("div", { class: "fs-item-sub" }, `${t.name} · ${fmtSize(t.size)}`),
        h("div", { class: "fs-row" }, ...transferActions(t)),
      ),
    ),
  );
  if (modalKey?.startsWith("offer:") && !offers.some(o => `offer:${o.id}` === modalKey)) closeModal(false);
  const next = offers.find(o => !dismissedOffers.has(o.id));
  if (next && $("fs-modal").classList.contains("hidden")) offerDialog(next);
}

function offerDialog(t: Transfer) {
  const c = contacts.find(x => x.id === t.contact);
  openModal(
    "Incoming file",
    [
      h(
        "dl",
        { class: "fs-dl" },
        h("dt", {}, "From"),
        h("dd", {}, t.contact_name || contactName(t.contact), c?.verified ? h("span", { class: "fs-badge ok" }, "✓ verified") : h("span", { class: "fs-badge warn" }, "not verified")),
        h("dt", {}, "File"),
        h("dd", { class: "fs-break" }, t.name),
        h("dt", {}, "Size"),
        h("dd", {}, fmtSize(t.size)),
        c ? h("dt", {}, "Fingerprint") : null,
        c ? h("dd", {}, h("code", { class: "fs-fp" }, c.fingerprint)) : null,
        t.sas !== null ? h("dt", {}, "Code") : null,
        t.sas !== null ? h("dd", {}, h("strong", { class: "fs-sas" }, String(t.sas).padStart(4, "0")), " — the sender sees the same code") : null,
        t.relayed !== null ? h("dt", {}, "Path") : null,
        t.relayed !== null ? h("dd", {}, t.relayed ? "via your relay" : "direct") : null,
      ),
    ],
    [
      {
        label: "Decline",
        run: async () => {
          await invoke("reject", { id: t.id, reason: "declined" });
          await refreshTransfers();
        },
      },
      {
        label: "Accept",
        kind: "primary",
        run: async () => {
          await invoke("accept", { id: t.id });
          await refreshTransfers();
        },
      },
    ],
    `offer:${t.id}`,
    () => dismissedOffers.add(t.id), // closed without deciding: keep inline card
  );
}

async function refreshTransfers() {
  try {
    const list = await invoke<Transfer[]>("list_transfers");
    transfers.clear();
    for (const t of list) transfers.set(t.id, t);
  } catch {
    return;
  }
  scheduleRender();
}

$("fs-clear-btn").addEventListener("click", async () => {
  try {
    await invoke("clear_finished");
  } catch (e) {
    toast(errText(e), "error");
  }
  await refreshTransfers();
});

// ─── Settings ────────────────────────────────────────────────────────

async function loadSettings() {
  try {
    settings = await invoke<SettingsView>("get_settings");
  } catch (e) {
    $("fs-set-msg").textContent = errText(e);
    return;
  }
  const s = settings;
  ($("fs-set-name") as HTMLInputElement).value = s.display_name;
  ($("fs-set-name") as HTMLInputElement).placeholder = s.effective_display_name;
  $("fs-set-name-hint").textContent = s.display_name ? "" : `Empty = your alias "${s.effective_display_name}".`;
  ($("fs-set-relay") as HTMLInputElement).value = s.relay_addr;
  ($("fs-set-token") as HTMLInputElement).value = "";
  const ts = $("fs-token-state");
  ts.textContent = s.relay_token_set ? "Token set" : "No token";
  ts.className = `fs-badge ${s.relay_token_set ? "ok" : ""}`;
  ($("fs-token-clear") as HTMLButtonElement).disabled = !s.relay_token_set;
  for (const r of document.querySelectorAll<HTMLInputElement>('input[name="fs-iroh"]')) r.checked = r.value === s.iroh_relay.mode;
  ($("fs-set-iroh-url") as HTMLInputElement).value = s.iroh_relay.mode === "custom" ? s.iroh_relay.url : "";
  syncIrohUrl();
  $("fs-dl-path").textContent = s.download_dir;
  ($("fs-dl-reset") as HTMLButtonElement).disabled = s.download_dir_is_default;
  const android = s.platform === "android";
  $("fs-dl-block").classList.toggle("hidden", android);
  $("fs-dl-android").classList.toggle("hidden", !android);
  ($("fs-set-auto") as HTMLInputElement).checked = s.auto_accept_verified;
}

function syncIrohUrl() {
  const mode = document.querySelector<HTMLInputElement>('input[name="fs-iroh"]:checked')?.value;
  ($("fs-set-iroh-url") as HTMLInputElement).classList.toggle("hidden", mode !== "custom");
}
for (const r of document.querySelectorAll<HTMLInputElement>('input[name="fs-iroh"]')) r.addEventListener("change", syncIrohUrl);

async function saveSettings(update: Record<string, unknown>, okMsg = "Saved") {
  const msg = $("fs-set-msg");
  try {
    settings = await invoke<SettingsView>("set_settings", { update });
    msg.textContent = okMsg;
    msg.className = "fs-msg ok";
    await loadSettings();
    void refreshIdentity();
  } catch (e) {
    msg.textContent = errText(e);
    msg.className = "fs-msg error";
  }
}

$("fs-settings").addEventListener("submit", e => {
  e.preventDefault();
  const mode = document.querySelector<HTMLInputElement>('input[name="fs-iroh"]:checked')?.value ?? "n0";
  const iroh_relay: IrohRelay =
    mode === "custom" ? { mode: "custom", url: ($("fs-set-iroh-url") as HTMLInputElement).value.trim() } : mode === "off" ? { mode: "off" } : { mode: "n0" };
  const token = ($("fs-set-token") as HTMLInputElement).value.trim();
  const update: Record<string, unknown> = {
    display_name: ($("fs-set-name") as HTMLInputElement).value,
    relay_addr: ($("fs-set-relay") as HTMLInputElement).value,
    iroh_relay,
    auto_accept_verified: ($("fs-set-auto") as HTMLInputElement).checked,
  };
  if (token) update.relay_token = token;
  void saveSettings(update);
});
$("fs-token-clear").addEventListener("click", () => void saveSettings({ clear_relay_token: true }, "Relay token cleared"));
$("fs-dl-choose").addEventListener("click", async () => {
  try {
    const dir = await invoke<string | null>("pick_download_folder");
    if (dir) await saveSettings({ download_dir: dir });
  } catch (e) {
    toast(errText(e), "error");
  }
});
$("fs-dl-reset").addEventListener("click", () => void saveSettings({ download_dir: "" }));
$("fs-dl-open").addEventListener("click", () => invoke("open_download_folder").catch(e => toast(errText(e), "error")));

// ─── Events ──────────────────────────────────────────────────────────

function onEvent(ev: ShareEvent) {
  switch (ev.type) {
    case "presence": {
      const c = contacts.find(x => x.id === ev.id);
      if (c) {
        c.presence = ev.state;
        c.last_seen = ev.last_seen;
        renderContacts();
        renderStatus();
      }
      break;
    }
    case "contact_request":
      void refreshContacts();
      break;
    case "transfer_progress": {
      const t = transfers.get(ev.id);
      if (t) {
        t.done = ev.done;
        if (ev.total) t.size = ev.total;
        trackSpeed(ev.id, ev.done);
        scheduleRender();
      }
      break;
    }
    case "invite_refused":
      toast(`File invite from ${contactName(ev.from)} refused: ${ev.reason}`, "error");
      break;
    case "text":
      break;
    case "transfer_cancelled":
      upsertTransfer(ev.transfer);
      if (ev.by_peer) toast(`${ev.transfer.contact_name || "Peer"} cancelled ${ev.transfer.name}`);
      scheduleRender();
      break;
    default:
      upsertTransfer(ev.transfer);
      if (ev.type === "transfer_started") speed.delete(ev.transfer.id);
      if (ev.type === "transfer_rejected" && ev.transfer.direction === "outgoing") toast(`${ev.transfer.contact_name} declined ${ev.transfer.name}`);
      scheduleRender();
  }
}

async function init() {
  document.body.dataset.tab = "files";
  showTab("files");
  try {
    await listen<ShareEvent>("share://event", e => onEvent(e.payload));
    await listen<NodeStatus>("share://status", async e => {
      status = e.payload;
      renderStatus();
      if (status.running) {
        await refreshIdentity();
        await refreshContacts();
        await refreshTransfers();
      }
    });
    await listen<{ id: string; location: string | null; error?: string }>("share://saved", e => {
      const t = transfers.get(e.payload.id);
      if (e.payload.location) {
        if (t) t.saved_to = e.payload.location;
        toast(`Saved to ${e.payload.location}`);
      } else if (e.payload.error) {
        toast(`Could not save to Downloads: ${e.payload.error}`, "error");
      }
      scheduleRender();
    });
    // Use take_shared_files' result, not the event payload, so cold-start
    // shares and shares that raced the listener are not lost.
    await listen("wzp-android://shared", () => void drainShared());
  } catch {
    // Outside Tauri.
    return;
  }
  try {
    status = await invoke<NodeStatus>("share_status");
  } catch {
    /* not managed: no identity */
  }
  renderStatus();
  if (status.running) {
    await refreshIdentity();
    await refreshContacts();
    await refreshTransfers();
  }
  await drainShared();
  void setupDragDrop();
  // Presence of contacts can change without events while starting up.
  setInterval(() => {
    if (status.running && document.body.dataset.tab === "contacts") void refreshContacts();
  }, 15000);
}

void init();
