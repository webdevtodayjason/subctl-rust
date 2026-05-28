// UI store — chrome-level state that the chat surface and status line
// both read. Theme + daemon status + active backend.

import { atom } from 'nanostores'

import { DEFAULT_THEME, type Theme } from '../theme.js'

export interface UiState {
  theme: Theme
  /** Lifecycle state of the daemon as the TUI sees it. */
  daemonStatus:
    | 'unknown'
    | 'starting'
    | 'running'
    | 'failed'
    | 'absent'
  /** Daemon version once /health returns. */
  daemonVersion: string | null
  /** Detail string for daemon failure modes. */
  daemonError: string | null
  /** Active thinking-partner backend; null = unknown / default. */
  backend: string | null
  /** Free-form status text shown in the chrome footer. */
  status: string
}

const initial: UiState = {
  theme: DEFAULT_THEME,
  daemonStatus: 'unknown',
  daemonVersion: null,
  daemonError: null,
  backend: null,
  status: 'ready'
}

export const $ui = atom<UiState>(initial)

export function patchUi(next: Partial<UiState>): void {
  $ui.set({ ...$ui.get(), ...next })
}
