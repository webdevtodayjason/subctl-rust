// Workers tab — table of registered workers, live-updated via SSE.
//
// Snapshots come from GET /api/evy/workers; live deltas land via the
// shared EventBus (worker_registered + worker_status_changed). Started
// timestamp is captured client-side on first sight, since neither the
// JSON snapshot nor the WorkerRegistered SSE event carries one.

import { escapeHTML, formatRelative } from '/app.js';

const STATUS_RANKS = {
  starting: 'starting',
  running: 'running',
  completed: 'completed',
  cancelled: 'cancelled',
  failed: 'failed',
};

function normaliseStatus(status) {
  // WorkerStatus is serialised either as a bare string ("running") or a
  // tagged variant ({ failed: "oom" }). Normalise to { kind, reason }.
  if (typeof status === 'string') {
    return { kind: status.toLowerCase(), reason: '' };
  }
  if (status && typeof status === 'object') {
    const k = Object.keys(status)[0] || 'unknown';
    return { kind: k.toLowerCase(), reason: String(status[k] ?? '') };
  }
  return { kind: 'unknown', reason: '' };
}

function statusBadge(status) {
  const s = normaliseStatus(status);
  const cls = STATUS_RANKS[s.kind] ? s.kind : '';
  const label = s.reason ? `${s.kind} · ${s.reason}` : s.kind;
  return `<span class="badge ${cls}">${escapeHTML(label)}</span>`;
}

function rowHTML(w) {
  return `
    <tr data-worker-id="${escapeHTML(w.id)}">
      <td class="mono">${escapeHTML(w.id)}</td>
      <td>${escapeHTML(w.provider)}</td>
      <td class="status-cell">${statusBadge(w.status)}</td>
      <td class="mono">${escapeHTML(w.mandate_id)}</td>
      <td class="started-cell">${escapeHTML(formatRelative(w.started))}</td>
    </tr>`;
}

export async function render(root, ctx) {
  root.innerHTML = `
    <section class="panel">
      <div class="panel-head">
        <h2>Workers <span class="panel-sub">live · /api/evy/workers + SSE</span></h2>
        <div class="toolbar">
          <span class="dim small" id="workers-count">0 workers</span>
          <button class="btn" id="workers-refresh">refresh</button>
        </div>
      </div>
      <table class="data-table">
        <thead>
          <tr>
            <th>Worker ID</th>
            <th>Provider</th>
            <th>Status</th>
            <th>Mandate ID</th>
            <th>Started</th>
          </tr>
        </thead>
        <tbody id="workers-body">
          <tr class="empty"><td colspan="5">loading…</td></tr>
        </tbody>
      </table>
    </section>
  `;

  const body = root.querySelector('#workers-body');
  const countEl = root.querySelector('#workers-count');
  const refreshBtn = root.querySelector('#workers-refresh');

  // Worker state map: id → { id, provider, mandate_id, status, started }
  const workers = new Map();

  function repaint() {
    if (workers.size === 0) {
      body.innerHTML = '<tr class="empty"><td colspan="5">no workers registered</td></tr>';
      countEl.textContent = '0 workers';
      return;
    }
    // Stable order: by started ASC (oldest first), falling back to id.
    const rows = [...workers.values()].sort((a, b) => {
      const ta = a.started ? a.started.getTime() : 0;
      const tb = b.started ? b.started.getTime() : 0;
      if (ta !== tb) return ta - tb;
      return a.id.localeCompare(b.id);
    });
    body.innerHTML = rows.map(rowHTML).join('');
    countEl.textContent = `${workers.size} worker${workers.size === 1 ? '' : 's'}`;
  }

  // Refresh the started-cell on a 5s tick so "Xs ago" stays accurate.
  const tickHandle = setInterval(() => {
    for (const w of workers.values()) {
      const cell = body.querySelector(`tr[data-worker-id="${CSS.escape(w.id)}"] .started-cell`);
      if (cell) cell.textContent = formatRelative(w.started);
    }
  }, 5000);

  async function fetchSnapshot() {
    try {
      const r = await fetch('/api/evy/workers');
      if (!r.ok) throw new Error(`http ${r.status}`);
      const list = await r.json();
      const now = new Date();
      for (const w of list) {
        const prev = workers.get(w.id);
        workers.set(w.id, {
          id: w.id,
          provider: w.provider,
          mandate_id: w.mandate_id,
          status: w.status,
          started: prev?.started ?? now,
        });
      }
      // Drop workers that disappeared from the snapshot.
      const fresh = new Set(list.map((w) => w.id));
      for (const id of [...workers.keys()]) {
        if (!fresh.has(id)) workers.delete(id);
      }
      repaint();
    } catch (err) {
      console.warn('[workers] snapshot failed', err);
      body.innerHTML = '<tr class="empty"><td colspan="5">failed to load workers</td></tr>';
    }
  }

  refreshBtn.addEventListener('click', fetchSnapshot);

  const unsub = ctx.bus.subscribe((ev) => {
    if (!ev || typeof ev !== 'object') return;
    if (ev.type === 'worker_registered') {
      workers.set(ev.worker_id, {
        id: ev.worker_id,
        provider: ev.provider,
        mandate_id: ev.mandate_id,
        status: 'starting',
        started: new Date(),
      });
      repaint();
    } else if (ev.type === 'worker_status_changed') {
      const w = workers.get(ev.worker_id);
      if (w) {
        w.status = ev.status;
        repaint();
      } else {
        // Status changed for a worker we haven't seen — refetch.
        fetchSnapshot();
      }
    }
  });

  // Cleanup on next render of a different tab — app shell wipes innerHTML
  // so attach an observer that fires once the root subtree is replaced.
  const observer = new MutationObserver(() => {
    if (!root.isConnected || !body.isConnected) {
      clearInterval(tickHandle);
      unsub();
      observer.disconnect();
    }
  });
  observer.observe(root.parentNode || document.body, { childList: true, subtree: true });

  await fetchSnapshot();
}
