// Theme palette ported from Hermes ui-tui's DARK_THEME. Kept narrow on
// purpose — we only carry the swatches we actually consume.
// Citation: hermes-agent/ui-tui/src/theme.ts:257-305.

export interface ThemeColors {
  primary: string
  accent: string
  border: string
  text: string
  muted: string

  label: string
  ok: string
  error: string
  warn: string

  prompt: string

  statusBg: string
  statusFg: string
  statusGood: string
  statusWarn: string
  statusBad: string
  statusCritical: string

  shellDollar: string
}

export interface ThemeBrand {
  name: string
  icon: string
  prompt: string
  welcome: string
  goodbye: string
  tool: string
  helpHeader: string
}

export interface Theme {
  color: ThemeColors
  brand: ThemeBrand
}

const BRAND: ThemeBrand = {
  name: 'Evy',
  icon: '◆',
  prompt: '❯',
  welcome: 'Type your message or /help for commands.',
  goodbye: 'Goodbye.',
  tool: '┊',
  helpHeader: 'Commands'
}

/**
 * Dark theme — matches Hermes's gold/bronze on dark palette so the visual
 * port lands without re-tuning. Light theme + skin overrides intentionally
 * dropped for v0.1; add when an operator complains.
 */
export const DARK_THEME: Theme = {
  color: {
    primary: '#FFD700',
    accent: '#FFBF00',
    border: '#CD7F32',
    text: '#FFF8DC',
    muted: '#CC9B1F',

    label: '#DAA520',
    ok: '#4caf50',
    error: '#ef5350',
    warn: '#ffa726',

    prompt: '#FFF8DC',

    statusBg: '#1a1a2e',
    statusFg: '#C0C0C0',
    statusGood: '#8FBC8F',
    statusWarn: '#FFD700',
    statusBad: '#FF8C00',
    statusCritical: '#FF6B6B',

    shellDollar: '#4dabf7'
  },
  brand: BRAND
}

export const DEFAULT_THEME = DARK_THEME
