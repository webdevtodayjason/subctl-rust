import { describe, test, expect } from 'vitest'

import {
  looksLikeSlashCommand,
  parseSlashCommand
} from '../src/slash/parser.js'
import { rewriteBackend, parseBackend } from '../src/slash/config.js'

describe('slash parser', () => {
  test('looksLikeSlashCommand requires leading slash + letter', () => {
    expect(looksLikeSlashCommand('/help')).toBe(true)
    expect(looksLikeSlashCommand('/Q')).toBe(true)
    expect(looksLikeSlashCommand('/')).toBe(false)
    expect(looksLikeSlashCommand('/123')).toBe(false)
    expect(looksLikeSlashCommand('hello /help')).toBe(false)
    expect(looksLikeSlashCommand('')).toBe(false)
  })

  test('parses commands with no args', () => {
    expect(parseSlashCommand('/quit')).toEqual({
      name: 'quit',
      args: '',
      argv: []
    })
  })

  test('parses commands with args and trims whitespace', () => {
    expect(parseSlashCommand('/backend lm-studio')).toEqual({
      name: 'backend',
      args: 'lm-studio',
      argv: ['lm-studio']
    })
    expect(parseSlashCommand('  /backend  lm-studio  ')).toEqual({
      name: 'backend',
      args: 'lm-studio',
      argv: ['lm-studio']
    })
  })

  test('lowercases the command name', () => {
    expect(parseSlashCommand('/HELP')?.name).toBe('help')
  })

  test('returns null for non-slash input', () => {
    expect(parseSlashCommand('hello')).toBe(null)
    expect(parseSlashCommand('/')).toBe(null)
  })

  test('multi-arg splits on any whitespace', () => {
    const out = parseSlashCommand('/foo one two\tthree')
    expect(out?.argv).toEqual(['one', 'two', 'three'])
  })
})

describe('config.toml backend rewrite', () => {
  const SEED = `[scheduler]
db_path = "/tmp/x.db"

[thinking_partner]
backend = "codex"
api_key_env = "ANTHROPIC_API_KEY"

[comms.http]
port = 8797
`

  test('parseBackend finds existing key', () => {
    const info = parseBackend(SEED)
    expect(info.backend).toBe('codex')
    expect(info.sectionStart).toBeGreaterThan(-1)
    expect(info.backendLine).toBeGreaterThan(info.sectionStart)
  })

  test('rewriteBackend updates an existing line in place', () => {
    const next = rewriteBackend(SEED, 'anthropic')
    const info = parseBackend(next)
    expect(info.backend).toBe('anthropic')
    expect(next).toContain('api_key_env = "ANTHROPIC_API_KEY"')
    expect(next).toContain('[comms.http]')
  })

  test('rewriteBackend inserts the line when section exists without key', () => {
    const noKey = `[thinking_partner]
api_key_env = "X"
`
    const next = rewriteBackend(noKey, 'lm-studio')
    expect(parseBackend(next).backend).toBe('lm-studio')
    expect(next).toContain('api_key_env = "X"')
  })

  test('rewriteBackend appends a new section when missing', () => {
    const noSection = `[scheduler]
db_path = "/tmp/x.db"
`
    const next = rewriteBackend(noSection, 'codex')
    expect(parseBackend(next).backend).toBe('codex')
    expect(next).toContain('[scheduler]')
  })

  test('rewriteBackend bootstraps a fresh file from empty', () => {
    const next = rewriteBackend('', 'lm-studio')
    expect(parseBackend(next).backend).toBe('lm-studio')
  })
})
