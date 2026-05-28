// `/backend` writes a single key (`backend = "..."`) under the
// `[thinking_partner]` table of ~/.config/subctl/v4/config.toml.
//
// We deliberately do NOT use a full TOML parser. The file format is
// well-structured (the installer seeds a known shape); we read the file,
// find the [thinking_partner] section, rewrite the backend line if it
// exists or append it inside the section, then write back atomically.
//
// Unknown keys outside [thinking_partner] are preserved verbatim — we
// only touch the lines we own.

import { readFile, writeFile, mkdir } from 'node:fs/promises'
import { dirname } from 'node:path'

import { CONFIG_TOML_PATH } from '../constants.js'

export type BackendChoice = 'lm-studio' | 'codex' | 'anthropic' | 'stub'

const KNOWN_BACKENDS: readonly BackendChoice[] = [
  'lm-studio',
  'codex',
  'anthropic',
  'stub'
]

export function isBackendChoice(s: string): s is BackendChoice {
  return (KNOWN_BACKENDS as readonly string[]).includes(s)
}

/**
 * Read the current backend setting. Returns null when the file is absent
 * or the key is unset (daemon default applies).
 */
export async function readBackend(
  path: string = CONFIG_TOML_PATH
): Promise<BackendChoice | null> {
  let raw: string
  try {
    raw = await readFile(path, 'utf8')
  } catch {
    return null
  }
  const result = parseBackend(raw)
  return result.backend
}

/**
 * Persist a backend choice. Creates the file (and parent dir) if needed,
 * inserts the section if missing, replaces the line if present.
 *
 * Returns the new file contents (useful for tests).
 */
export async function writeBackend(
  choice: BackendChoice,
  path: string = CONFIG_TOML_PATH
): Promise<string> {
  let raw = ''
  try {
    raw = await readFile(path, 'utf8')
  } catch {
    // Fall through — we'll create from scratch.
  }
  const next = rewriteBackend(raw, choice)
  await mkdir(dirname(path), { recursive: true })
  await writeFile(path, next, 'utf8')
  return next
}

interface ParseResult {
  backend: BackendChoice | null
  sectionStart: number // line index where [thinking_partner] sits, -1 if absent
  sectionEnd: number // exclusive end (line index of next [section] header, or EOF)
  backendLine: number // line index of `backend = …` inside the section, -1 if missing
}

/** Lightweight scan — does NOT support inline tables or multiline strings. */
export function parseBackend(raw: string): ParseResult {
  const lines = raw.split('\n')
  let sectionStart = -1
  let sectionEnd = lines.length
  let backendLine = -1
  let backend: BackendChoice | null = null

  for (let i = 0; i < lines.length; i++) {
    const ln = lines[i]
    if (ln === undefined) {
      continue
    }
    const trimmed = ln.trim()
    if (sectionStart === -1) {
      if (trimmed === '[thinking_partner]') {
        sectionStart = i
      }
    } else if (/^\[[^\]]+\]$/.test(trimmed)) {
      sectionEnd = i
      break
    } else {
      const m = /^\s*backend\s*=\s*"([^"]*)"\s*(#.*)?$/.exec(ln)
      if (m && m[1] !== undefined) {
        backendLine = i
        if (isBackendChoice(m[1])) {
          backend = m[1]
        }
      }
    }
  }
  return { backend, sectionStart, sectionEnd, backendLine }
}

export function rewriteBackend(raw: string, choice: BackendChoice): string {
  const lines = raw === '' ? [] : raw.split('\n')
  const info = parseBackend(raw)
  const newLine = `backend = "${choice}"`

  if (info.sectionStart === -1) {
    // Append a fresh [thinking_partner] block at the end. Insert a leading
    // blank line if the file is non-empty so we don't smash against the
    // previous section.
    const head: string[] = []
    if (lines.length > 0 && lines[lines.length - 1] !== '') {
      head.push('')
    }
    head.push('[thinking_partner]', newLine, '')
    return [...lines, ...head].join('\n')
  }

  if (info.backendLine !== -1) {
    lines[info.backendLine] = newLine
    return lines.join('\n')
  }

  // Section exists, key doesn't — insert right after the header.
  lines.splice(info.sectionStart + 1, 0, newLine)
  return lines.join('\n')
}
