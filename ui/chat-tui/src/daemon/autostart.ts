// Daemon autostart helper. Runs at TUI launch to make sure the v4 daemon
// is loaded before we try to chat.
//
// Sequence (per /Users/sem/.claude/plans/parallel-honking-wind.md §Daemon-offline):
//   1. `launchctl list | grep -q com.subctl.evy-v4`
//   2. If not loaded → `launchctl load <plist>`
//   3. Poll GET /health for up to 5s, every 250ms
//   4. Surface the result
//
// We deliberately use node:child_process here because Node doesn't have
// Bun.spawn — note this in the persona override. argv is passed as an
// array, never shell-interpolated.

import { spawn, spawnSync } from 'node:child_process'

import { DAEMON_LABEL, DAEMON_PLIST_PATH } from '../constants.js'
import { getHealth } from '../gateway/client.js'

export interface DaemonState {
  /** `loaded` = launchctl knows about it; `running` = also responds to /health. */
  status: 'running' | 'loaded' | 'absent' | 'failed'
  /** Diagnostic detail surfaced to the operator on failure. */
  detail?: string
  /** Daemon version reported by /health, when running. */
  version?: string
}

/** True when `launchctl list` shows our daemon label. */
export function isDaemonLoaded(): boolean {
  // `launchctl list` exit code is 0 even when the label is missing — we
  // have to filter the output ourselves.
  const res = spawnSync('launchctl', ['list'], { encoding: 'utf8' })
  if (res.status !== 0) {
    return false
  }
  return res.stdout
    .split('\n')
    .some((line) => line.includes(DAEMON_LABEL))
}

/**
 * Issue `launchctl load <plist>`. Doesn't wait for /health to come back —
 * caller is expected to poll separately.
 *
 * Returns the spawn exit code (0 = success, non-zero = error).
 */
export function loadDaemon(plistPath: string = DAEMON_PLIST_PATH): {
  ok: boolean
  detail?: string
} {
  // `launchctl load` is idempotent-ish on macOS: re-loading a loaded plist
  // emits a "Load failed: 5: Input/output error" but the daemon is fine.
  // We treat any non-zero exit as a soft warning; the /health poll is
  // what actually decides.
  const res = spawnSync('launchctl', ['load', plistPath], {
    encoding: 'utf8'
  })
  if (res.status === 0) {
    return { ok: true }
  }
  return {
    ok: false,
    detail: (res.stderr || res.stdout || `exit ${res.status}`).trim()
  }
}

/**
 * Poll /health every `intervalMs` for up to `timeoutMs`. Resolves with
 * the response on first success, throws on timeout.
 */
export async function waitForHealth(opts?: {
  url?: string
  intervalMs?: number
  timeoutMs?: number
  signal?: AbortSignal
}): Promise<{ ok: true; version: string }> {
  const interval = opts?.intervalMs ?? 250
  const deadline = Date.now() + (opts?.timeoutMs ?? 5000)
  let lastErr: unknown = null

  while (Date.now() < deadline) {
    if (opts?.signal?.aborted) {
      throw new Error('aborted')
    }
    try {
      const h = await getHealth({ url: opts?.url, signal: opts?.signal })
      if (h.ok) {
        return { ok: true, version: h.version }
      }
    } catch (err) {
      lastErr = err
    }
    await sleep(interval, opts?.signal)
  }

  throw new Error(
    `daemon failed to become healthy within ${
      opts?.timeoutMs ?? 5000
    }ms (${String(lastErr ?? 'no response')})`
  )
}

/** Drive the full check → load → poll sequence. */
export async function ensureDaemonRunning(opts?: {
  url?: string
  plistPath?: string
  timeoutMs?: number
  signal?: AbortSignal
}): Promise<DaemonState> {
  // Fast path — /health works without touching launchctl at all.
  // A 2-second hard timeout guards against a hung daemon process (the
  // /health route exists but never replies) — otherwise the entire boot
  // sequence would hang waiting for fetch to give up.
  const fastTimeout = AbortSignal.timeout(2000)
  const fastSignal = opts?.signal
    ? mergeAbortSignals([opts.signal, fastTimeout])
    : fastTimeout
  try {
    const h = await getHealth({ url: opts?.url, signal: fastSignal })
    if (h.ok) {
      return { status: 'running', version: h.version }
    }
  } catch {
    // expected on a cold start
  }

  const loaded = isDaemonLoaded()
  if (!loaded) {
    const loadResult = loadDaemon(opts?.plistPath)
    if (!loadResult.ok) {
      // Don't bail yet — the plist may have been hand-loaded; poll /health
      // and let that be the source of truth.
    }
  }

  try {
    const h = await waitForHealth({
      url: opts?.url,
      timeoutMs: opts?.timeoutMs ?? 5000,
      signal: opts?.signal
    })
    return { status: 'running', version: h.version }
  } catch (err) {
    return {
      status: loaded ? 'loaded' : 'failed',
      detail: String(err)
    }
  }
}

function sleep(ms: number, signal?: AbortSignal): Promise<void> {
  return new Promise((resolve, reject) => {
    const t = setTimeout(resolve, ms)
    if (signal) {
      const onAbort = () => {
        clearTimeout(t)
        reject(new Error('aborted'))
      }
      if (signal.aborted) {
        onAbort()
      } else {
        signal.addEventListener('abort', onAbort, { once: true })
      }
    }
  })
}

// Suppress unused import warning — we'll wire spawn() in when we need to
// surface daemon stderr (Tier 2).
void spawn

/**
 * Merge an arbitrary number of AbortSignals into one. Aborts as soon as
 * any input aborts.  Used by `ensureDaemonRunning` to bound the fast-path
 * health check while still honouring caller cancellation.
 */
function mergeAbortSignals(signals: readonly AbortSignal[]): AbortSignal {
  const ctrl = new AbortController()
  const onAbort = (sig: AbortSignal) => {
    try {
      ctrl.abort(sig.reason)
    } catch {
      // ignore
    }
  }
  for (const sig of signals) {
    if (sig.aborted) {
      onAbort(sig)
      return ctrl.signal
    }
    sig.addEventListener('abort', () => onAbort(sig), { once: true })
  }
  return ctrl.signal
}
