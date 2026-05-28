// Smoke — confirms the test runner picks up files and modules import.
import { describe, test, expect } from 'vitest'

import { VERSION } from '../src/constants.js'
import { DEFAULT_THEME } from '../src/theme.js'

describe('smoke', () => {
  test('VERSION is set', () => {
    expect(VERSION).toMatch(/^\d+\.\d+\.\d+$/)
  })

  test('DEFAULT_THEME has the swatches we render', () => {
    expect(DEFAULT_THEME.color.primary).toMatch(/^#[0-9A-F]{6}$/i)
    expect(DEFAULT_THEME.brand.icon.length).toBeGreaterThan(0)
  })
})
