// Policy tab — pretty, foldable JSON tree of the loaded Policy.
//
// Read-only. Snapshot from /api/evy/policy; refresh button reloads. The
// tree uses <details>/<summary> for native fold state — no JS handling
// for expand/collapse, and the browser remembers it across re-renders.

import { escapeHTML } from '/app.js';

function isPrimitive(v) {
  return v === null || typeof v !== 'object';
}

function primitiveHTML(value) {
  if (value === null) return '<span class="null">null</span>';
  switch (typeof value) {
    case 'string':
      return `<span class="str">"${escapeHTML(value)}"</span>`;
    case 'number':
      return `<span class="num">${escapeHTML(String(value))}</span>`;
    case 'boolean':
      return `<span class="bool">${value}</span>`;
    default:
      return `<span>${escapeHTML(String(value))}</span>`;
  }
}

function renderNode(value, key, depth) {
  // Top-level objects + arrays default to expanded; nested ones collapse
  // by default for scannability on larger policies.
  const openAttr = depth < 1 ? ' open' : '';
  const labelPrefix = key !== null
    ? `<span class="key">${escapeHTML(key)}</span><span class="punct">:</span> `
    : '';

  if (isPrimitive(value)) {
    return `<div class="leaf">${labelPrefix}${primitiveHTML(value)}</div>`;
  }

  if (Array.isArray(value)) {
    if (value.length === 0) {
      return `<div class="leaf">${labelPrefix}<span class="punct">[]</span></div>`;
    }
    const inner = value
      .map((v, i) => renderNode(v, String(i), depth + 1))
      .join('');
    return `
      <details${openAttr}>
        <summary>${labelPrefix}<span class="punct">[</span><span class="dim small">${value.length}</span><span class="punct">]</span></summary>
        ${inner}
      </details>`;
  }

  const keys = Object.keys(value);
  if (keys.length === 0) {
    return `<div class="leaf">${labelPrefix}<span class="punct">{}</span></div>`;
  }
  const inner = keys
    .map((k) => renderNode(value[k], k, depth + 1))
    .join('');
  return `
    <details${openAttr}>
      <summary>${labelPrefix}<span class="punct">{</span><span class="dim small">${keys.length}</span><span class="punct">}</span></summary>
      ${inner}
    </details>`;
}

export async function render(root /* ctx unused: snapshot is sufficient */) {
  root.innerHTML = `
    <section class="panel">
      <div class="panel-head">
        <h2>Policy <span class="panel-sub">read-only · loaded policy from /api/evy/policy</span></h2>
        <div class="toolbar">
          <button class="btn" id="policy-expand">expand all</button>
          <button class="btn" id="policy-collapse">collapse all</button>
          <button class="btn" id="policy-refresh">refresh</button>
        </div>
      </div>
      <div class="policy-tree" id="policy-tree"><span class="dim">loading…</span></div>
    </section>
  `;

  const treeEl = root.querySelector('#policy-tree');

  async function load() {
    try {
      const r = await fetch('/api/evy/policy');
      if (!r.ok) throw new Error(`http ${r.status}`);
      const policy = await r.json();
      treeEl.innerHTML = renderNode(policy, null, 0);
    } catch (err) {
      console.warn('[policy] load failed', err);
      treeEl.innerHTML = `<span class="dim">failed to load policy: ${escapeHTML(String(err))}</span>`;
    }
  }

  root.querySelector('#policy-refresh').addEventListener('click', load);
  root.querySelector('#policy-expand').addEventListener('click', () => {
    treeEl.querySelectorAll('details').forEach((d) => { d.open = true; });
  });
  root.querySelector('#policy-collapse').addEventListener('click', () => {
    treeEl.querySelectorAll('details').forEach((d) => { d.open = false; });
  });

  await load();
}
