// Events tab — rolling feed of DaemonEvents from the SSE stream.
//
// Capped at MAX_EVENTS rows (DOM nodes evicted oldest-first, mirrors the
// in-memory ring) so a chatty daemon doesn't balloon the page. Filter
// input is substring-matched against the serialised JSON of each event,
// so it works across every field without bespoke-per-variant wiring.

import { escapeHTML, formatClock } from '/app.js';

const MAX_EVENTS = 200;

const KNOWN_TYPES = new Set([
  'daemon_booted',
  'worker_registered',
  'worker_status_changed',
  'scheduler_fired',
  'policy_checked',
  'heartbeat',
]);

function summary(ev) {
  switch (ev.type) {
    case 'daemon_booted':
      return `version=${ev.version} providers=[${(ev.providers || []).join(',')}]`;
    case 'worker_registered':
      return `worker=${ev.worker_id} provider=${ev.provider} mandate=${ev.mandate_id}`;
    case 'worker_status_changed': {
      const s = typeof ev.status === 'string' ? ev.status : JSON.stringify(ev.status);
      return `worker=${ev.worker_id} status=${s}`;
    }
    case 'scheduler_fired': {
      const o = typeof ev.outcome === 'string' ? ev.outcome : JSON.stringify(ev.outcome);
      return `job=${ev.job_id} run=${ev.run_id} outcome=${o}`;
    }
    case 'policy_checked':
      return `outcome=${ev.outcome_kind} cmd="${ev.command}"`;
    case 'heartbeat':
      return `providers_healthy=${ev.providers_healthy}`;
    default:
      return JSON.stringify(ev);
  }
}

function rowHTML(ev, seenAt) {
  const tp = ev.type || 'unknown';
  const cls = KNOWN_TYPES.has(tp) ? `ev-${tp}` : 'ev-unknown';
  const extra = tp === 'policy_checked' && ev.outcome_kind === 'deny' ? ' deny' : '';
  return `
    <div class="event-row ${cls}${extra}">
      <span class="ev-ts">${escapeHTML(formatClock(seenAt))}</span>
      <span class="ev-type">${escapeHTML(tp)}</span>
      <span class="ev-detail">${escapeHTML(summary(ev))}</span>
    </div>`;
}

export async function render(root, ctx) {
  root.innerHTML = `
    <section class="panel">
      <div class="panel-head">
        <h2>Events <span class="panel-sub">live · last ${MAX_EVENTS} from /api/evy/events</span></h2>
        <div class="toolbar">
          <input class="input" id="ev-filter" placeholder="filter (substring match)…" />
          <button class="btn" id="ev-clear">clear</button>
        </div>
      </div>
      <div class="events-feed" id="ev-feed">
        <div class="events-empty">waiting for events…</div>
      </div>
    </section>
  `;

  const feed = root.querySelector('#ev-feed');
  const filterInput = root.querySelector('#ev-filter');
  const clearBtn = root.querySelector('#ev-clear');

  // Ring buffer of recent events (newest at the END to match DOM order).
  const ring = [];
  let filter = '';

  function matches(entry) {
    if (!filter) return true;
    return entry.haystack.includes(filter);
  }

  function repaint() {
    if (ring.length === 0) {
      feed.innerHTML = '<div class="events-empty">no events yet</div>';
      return;
    }
    const visible = ring.filter(matches);
    if (visible.length === 0) {
      feed.innerHTML = '<div class="events-empty">no events match filter</div>';
      return;
    }
    // Newest at top.
    feed.innerHTML = visible
      .slice()
      .reverse()
      .map((entry) => rowHTML(entry.ev, entry.seenAt))
      .join('');
  }

  function pushEvent(ev) {
    const entry = {
      ev,
      seenAt: new Date(),
      haystack: `${ev.type || ''} ${JSON.stringify(ev)}`.toLowerCase(),
    };
    ring.push(entry);
    if (ring.length > MAX_EVENTS) ring.shift();
    repaint();
  }

  filterInput.addEventListener('input', () => {
    filter = filterInput.value.trim().toLowerCase();
    repaint();
  });
  clearBtn.addEventListener('click', () => {
    ring.length = 0;
    repaint();
  });

  const unsub = ctx.bus.subscribe(pushEvent);

  // If app.js already saw an event before this tab mounted, replay it
  // so the feed isn't blank on first paint.
  if (ctx.bus.lastEvent) pushEvent(ctx.bus.lastEvent);

  const observer = new MutationObserver(() => {
    if (!root.isConnected || !feed.isConnected) {
      unsub();
      observer.disconnect();
    }
  });
  observer.observe(root.parentNode || document.body, { childList: true, subtree: true });
}
