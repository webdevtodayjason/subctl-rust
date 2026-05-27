// Scheduler tab — registered jobs, last-run captured from SSE.
//
// JobSummary doesn't carry last-run / outcome on the JSON snapshot, so
// we listen for scheduler_fired events on the shared bus and stash the
// most recent (timestamp + outcome) per job-id client-side. Refresh
// button + 30s auto-refresh keep the snapshot fresh.

import { escapeHTML, formatRelative } from '/app.js';

const AUTO_REFRESH_MS = 30_000;

function outcomeBadge(outcome) {
  if (!outcome) return '<span class="dim">—</span>';
  const kind = typeof outcome === 'string'
    ? outcome.toLowerCase()
    : (Object.keys(outcome)[0] || 'unknown').toLowerCase();
  return `<span class="badge ${kind}">${escapeHTML(kind)}</span>`;
}

function rowHTML(job, lastRun) {
  const enabledMark = job.enabled
    ? '<span class="dim" title="armed">●</span>'
    : '<span class="mute" title="disabled">○</span>';
  return `
    <tr data-job-id="${escapeHTML(job.id)}">
      <td>${enabledMark} ${escapeHTML(job.name)}</td>
      <td class="mono">${escapeHTML(job.cron_expr)}</td>
      <td class="mono small">${escapeHTML(job.action_kind)}</td>
      <td class="last-run-cell">${escapeHTML(lastRun ? formatRelative(lastRun.at) : '—')}</td>
      <td class="outcome-cell">${outcomeBadge(lastRun?.outcome)}</td>
    </tr>`;
}

export async function render(root, ctx) {
  root.innerHTML = `
    <section class="panel">
      <div class="panel-head">
        <h2>Scheduler <span class="panel-sub">last-run state from SSE · auto-refresh every 30s</span></h2>
        <div class="toolbar">
          <span class="dim small" id="sched-count">0 jobs</span>
          <button class="btn" id="sched-refresh">refresh</button>
        </div>
      </div>
      <table class="data-table">
        <thead>
          <tr>
            <th>Name</th>
            <th>Cron</th>
            <th>Action</th>
            <th>Last run</th>
            <th>Outcome</th>
          </tr>
        </thead>
        <tbody id="sched-body">
          <tr class="empty"><td colspan="5">loading…</td></tr>
        </tbody>
      </table>
    </section>
  `;

  const body = root.querySelector('#sched-body');
  const countEl = root.querySelector('#sched-count');
  const refreshBtn = root.querySelector('#sched-refresh');

  let jobs = [];
  const lastRunByJob = new Map(); // job_id → { at: Date, outcome }

  function repaint() {
    if (jobs.length === 0) {
      body.innerHTML = '<tr class="empty"><td colspan="5">no jobs registered</td></tr>';
      countEl.textContent = '0 jobs';
      return;
    }
    body.innerHTML = jobs
      .map((j) => rowHTML(j, lastRunByJob.get(j.id)))
      .join('');
    countEl.textContent = `${jobs.length} job${jobs.length === 1 ? '' : 's'}`;
  }

  // Tick the relative-time column so "5s ago" doesn't go stale silently.
  const tickHandle = setInterval(() => {
    for (const [jobId, last] of lastRunByJob) {
      const cell = body.querySelector(`tr[data-job-id="${CSS.escape(jobId)}"] .last-run-cell`);
      if (cell) cell.textContent = formatRelative(last.at);
    }
  }, 5000);

  async function fetchSnapshot() {
    try {
      const r = await fetch('/api/evy/scheduler/jobs');
      if (!r.ok) throw new Error(`http ${r.status}`);
      jobs = await r.json();
      repaint();
    } catch (err) {
      console.warn('[scheduler] snapshot failed', err);
      body.innerHTML = '<tr class="empty"><td colspan="5">failed to load jobs</td></tr>';
    }
  }

  refreshBtn.addEventListener('click', fetchSnapshot);
  const autoHandle = setInterval(fetchSnapshot, AUTO_REFRESH_MS);

  const unsub = ctx.bus.subscribe((ev) => {
    if (ev?.type !== 'scheduler_fired') return;
    lastRunByJob.set(ev.job_id, { at: new Date(), outcome: ev.outcome });
    repaint();
  });

  const observer = new MutationObserver(() => {
    if (!root.isConnected || !body.isConnected) {
      clearInterval(autoHandle);
      clearInterval(tickHandle);
      unsub();
      observer.disconnect();
    }
  });
  observer.observe(root.parentNode || document.body, { childList: true, subtree: true });

  await fetchSnapshot();
}
