"use strict";

const tauri = window.__TAURI__;
const invoke = (cmd, args) => (tauri ? tauri.core.invoke(cmd, args) : Promise.resolve(null));

const $ = (id) => document.getElementById(id);
const els = {
  arm: $("arm"),
  armTitle: $("armTitle"),
  armSub: $("armSub"),
  toggle: $("switch"),
  cancel: $("cancel"),
  list: $("list"),
  empty: $("empty"),
  count: $("count"),
  dry: $("dry"),
  confirm: $("confirm"),
  toast: $("toast"),
};

let state = null;
const rows = new Map();

// ---------- formatting ----------

const pad = (n) => String(n).padStart(2, "0");

function duration(ms) {
  const s = Math.max(0, Math.floor(ms / 1000));
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  return h > 0 ? `${h}:${pad(m)}:${pad(s % 60)}` : `${m}:${pad(s % 60)}`;
}

function ago(ms) {
  const m = Math.floor(ms / 60000);
  if (m < 1) return "just now";
  if (m < 60) return `${m}m ago`;
  return `${Math.floor(m / 60)}h ago`;
}

function clock(epoch) {
  return new Date(epoch).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

function esc(text) {
  return String(text).replace(/[&<>"']/g, (c) => `&#${c.charCodeAt(0)};`);
}

const plural = (n, word) => `${n} ${word}${n === 1 ? "" : "s"}`;

// ---------- sessions ----------

function statusText(s, now) {
  switch (s.state) {
    case "working":
      return duration(now - s.since);
    case "goal":
      return "goal active";
    case "limit":
      return s.until ? `resets ${clock(s.until)}` : "usage limit";
    case "offline":
      return "connection lost";
    case "input":
      return "needs input";
    case "error":
      return "stopped";
    default:
      return `done ${ago(now - s.since)}`;
  }
}

function subtitle(s) {
  const parts = [];
  if (s.label !== s.title) parts.push(s.label);
  if (s.agents > 1) parts.push(`${s.agents} agents`);
  if (s.pid) parts.push(`PID ${s.pid}`);
  return parts.join(" · ");
}

function rowHtml(s) {
  return `${harnessIcon(s.icon)}
    <div class="info">
      <div class="title">${esc(s.title)}</div>
      <div class="sub">${esc(subtitle(s))}</div>
    </div>
    <div class="status ${s.state}"><span class="time"></span><span class="dot"></span></div>`;
}

function renderList(sessions) {
  const keys = new Set(sessions.map((s) => s.key));
  for (const [key, el] of rows) {
    if (!keys.has(key)) {
      el.remove();
      rows.delete(key);
    }
  }
  sessions.forEach((s, i) => {
    let el = rows.get(s.key);
    if (!el) {
      el = document.createElement("li");
      rows.set(s.key, el);
    }
    const sig = JSON.stringify([s.icon, s.title, s.label, s.pid, s.state, s.agents]);
    if (el.dataset.sig !== sig) {
      el.className = `row${s.blocking || s.state === "input" ? "" : " quiet"}`;
      el.innerHTML = rowHtml(s);
      el.dataset.sig = sig;
    }
    el.session = s;
    if (els.list.children[i] !== el) els.list.insertBefore(el, els.list.children[i] || null);
  });
  els.empty.hidden = sessions.length > 0;
  els.count.textContent = state.active > 0 ? `${state.active} active` : "";
}

// ---------- shutdown card ----------

function renderArm(now) {
  const s = state;
  const left = s.deadline ? duration(s.deadline - now + 999) : "";
  const countdown = s.phase === "countdown";

  els.arm.classList.toggle("on", s.armed && !countdown);
  els.arm.classList.toggle("countdown", countdown);
  els.toggle.hidden = countdown;
  els.cancel.hidden = !countdown;
  els.toggle.setAttribute("aria-checked", String(s.armed));
  els.toggle.disabled = !s.can_arm;
  els.toggle.title = s.can_arm ? "" : "Available once an agent is working";

  let title = "Shut down when done";
  let sub;
  switch (s.phase) {
    case "watching": {
      const working = s.sessions.filter((x) => x.state === "working" || x.state === "goal").length;
      sub = working > 0
        ? `Waiting for ${plural(working, "agent")} to finish`
        : `Waiting for ${plural(s.active, "agent")} to resume`;
      break;
    }
    case "settling":
      sub = `All agents finished · confirming ${left}`;
      break;
    case "offline":
      sub = "All agents finished · waiting for network";
      break;
    case "countdown":
      title = `Shutting down in ${left}`;
      sub = "All agents have finished";
      break;
    default:
      sub = s.can_arm
        ? "Turns off the PC when agents finish"
        : "Available once an agent is working";
  }
  els.armTitle.textContent = title;
  els.armSub.textContent = sub;
}

// ---------- render loop ----------

function render() {
  if (!state) return;
  els.dry.hidden = !state.dry_run;
  renderList(state.sessions);
  tick();
  showToast(state.notice);
}

function tick() {
  if (!state) return;
  const now = Date.now();
  renderArm(now);
  for (const el of rows.values()) {
    const time = el.querySelector(".time");
    const text = statusText(el.session, now);
    if (time.textContent !== text) time.textContent = text;
  }
}

let toastTimer = null;
let lastNotice = null;
function showToast(text) {
  if (!text || text === lastNotice) return;
  lastNotice = text;
  els.toast.textContent = text;
  els.toast.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => {
    els.toast.hidden = true;
    lastNotice = null;
  }, 6000);
}

function setState(next) {
  if (!next) return;
  state = next;
  render();
}

// ---------- actions ----------

els.toggle.addEventListener("click", () => {
  if (!state || els.toggle.disabled) return;
  invoke("set_armed", { armed: !state.armed });
});

els.cancel.addEventListener("click", () => invoke("set_armed", { armed: false }));

$("hide").addEventListener("click", () => invoke("hide_window"));

$("quit").addEventListener("click", () => {
  if (state && state.armed) els.confirm.hidden = false;
  else invoke("quit_app");
});

$("keep").addEventListener("click", () => {
  els.confirm.hidden = true;
});

$("quitAnyway").addEventListener("click", () => invoke("quit_app"));

document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") {
    if (!els.confirm.hidden) els.confirm.hidden = true;
    else invoke("hide_window");
  }
});

document.addEventListener("contextmenu", (e) => e.preventDefault());

if (tauri) {
  tauri.event.listen("state", (e) => setState(e.payload));
  invoke("get_state").then(setState);
}

setInterval(tick, 1000);
