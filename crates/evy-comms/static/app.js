// evy v4 operator console — main shell.
//
// Responsibilities:
//   • hash-based router for the 4 operator tabs
//   • lazy-load per-tab JS modules on first visit
//   • single shared EventSource subscribed to /api/evy/events
//   • surface daemon version + connection status in the topbar
//
// No framework, no build step. Tabs receive a small `ctx` object
// exposing the event bus + helpers so they can register listeners
// without each opening its own EventSource.

const TABS = ['workers', 'scheduler', 'events', 'policy'];
const DEFAULT_TAB = 'workers';

// ─── EventBus: fans the SSE stream out to per-tab subscribers ─────────

class EventBus {
  constructor() {
    this.listeners = new Set();
    this.lastEvent = null;
  }
  subscribe(fn) {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  }
  emit(ev) {
    this.lastEvent = ev;
    for (const fn of this.listeners) {
      try { fn(ev); } catch (err) { console.error('[evy] event handler error', err); }
    }
  }
}

const bus = new EventBus();

// ─── connection status pill ──────────────────────────────────────────

const statusEl = document.getElementById('status-pill');
const statusDotEl = statusEl?.querySelector('.dot');
const statusTextEl = statusEl?.querySelector('.status-text');
const pulseEl = document.getElementById('pulse-dot');

function setStatus(state, text) {
  if (!statusDotEl || !statusTextEl) return;
  statusDotEl.className = `dot ${state}`;
  statusTextEl.textContent = text;
  if (pulseEl) pulseEl.className = `pulse-dot ${state}`;
}

// ─── version chip ────────────────────────────────────────────────────

async function loadVersion() {
  try {
    const r = await fetch('/api/version');
    if (!r.ok) throw new Error(`http ${r.status}`);
    const j = await r.json();
    const chip = document.getElementById('version-chip');
    if (chip && j.version) chip.textContent = `v${j.version}`;
  } catch (err) {
    console.warn('[evy] /api/version failed', err);
  }
}

// ─── shared SSE connection ───────────────────────────────────────────

let sse = null;
let sseReconnectTimer = null;

function connectSSE() {
  if (sse) {
    try { sse.close(); } catch (_) { /* ignore */ }
  }
  setStatus('stale', 'connecting…');
  try {
    sse = new EventSource('/api/evy/events');
  } catch (err) {
    console.error('[evy] EventSource construct failed', err);
    scheduleReconnect();
    return;
  }
  sse.onopen = () => setStatus('live', 'connected');
  sse.onerror = (err) => {
    console.warn('[evy] SSE error — reconnecting', err);
    setStatus('dead', 'disconnected');
    scheduleReconnect();
  };
  sse.onmessage = (ev) => {
    if (!ev.data) return;
    let parsed;
    try { parsed = JSON.parse(ev.data); }
    catch (e) {
      console.warn('[evy] non-JSON SSE frame', ev.data);
      return;
    }
    bus.emit(parsed);
  };
}

function scheduleReconnect() {
  if (sseReconnectTimer) return;
  sseReconnectTimer = setTimeout(() => {
    sseReconnectTimer = null;
    connectSSE();
  }, 2000);
}

// ─── router ──────────────────────────────────────────────────────────

const contentEl = document.getElementById('content');
const navEls = Array.from(document.querySelectorAll('#sidebar-nav .nav-btn'));
const tabCache = new Map(); // tab name → loaded module's render fn

async function loadTabModule(name) {
  if (tabCache.has(name)) return tabCache.get(name);
  const mod = await import(`/tabs/${name}.js`);
  if (typeof mod.render !== 'function') {
    throw new Error(`tab module ${name} has no render() export`);
  }
  tabCache.set(name, mod.render);
  return mod.render;
}

async function showTab(name) {
  if (!TABS.includes(name)) name = DEFAULT_TAB;
  for (const btn of navEls) {
    btn.classList.toggle('active', btn.dataset.tab === name);
  }
  contentEl.innerHTML = `<div class="panel empty"><p class="dim">loading ${name}…</p></div>`;
  try {
    const render = await loadTabModule(name);
    contentEl.innerHTML = '';
    await render(contentEl, { bus, setStatus });
  } catch (err) {
    console.error(`[evy] tab ${name} failed to render`, err);
    contentEl.innerHTML =
      `<div class="panel"><h2>error</h2><p class="dim">${escapeHTML(String(err))}</p></div>`;
  }
}

function currentTab() {
  const hash = (location.hash || '').replace(/^#/, '');
  return TABS.includes(hash) ? hash : DEFAULT_TAB;
}

window.addEventListener('hashchange', () => showTab(currentTab()));

// ─── helpers ─────────────────────────────────────────────────────────

export function escapeHTML(s) {
  return String(s)
    .replaceAll('&', '&amp;')
    .replaceAll('<', '&lt;')
    .replaceAll('>', '&gt;')
    .replaceAll('"', '&quot;')
    .replaceAll("'", '&#39;');
}

export function formatRelative(date) {
  if (!date) return '—';
  const d = date instanceof Date ? date : new Date(date);
  if (Number.isNaN(d.getTime())) return '—';
  const diff = Math.max(0, Date.now() - d.getTime());
  const s = Math.floor(diff / 1000);
  if (s < 60) return `${s}s ago`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ago`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h ago`;
  return `${Math.floor(h / 24)}d ago`;
}

export function formatClock(date) {
  const d = date instanceof Date ? date : new Date(date);
  if (Number.isNaN(d.getTime())) return '—';
  return d.toLocaleTimeString([], { hour12: false });
}

// ─── boot ────────────────────────────────────────────────────────────

loadVersion();
connectSSE();
if (!location.hash) location.hash = `#${DEFAULT_TAB}`;
showTab(currentTab());
