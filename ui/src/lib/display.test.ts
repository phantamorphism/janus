import { describe, expect, it } from 'vitest'
import { credentialErrorText, formatRetain } from './format'
import { statusTone } from './tone'

describe('display helpers', () => {
  it('colours upstream statuses by class', () => {
    expect([200, 301, 404, 429, 503].map(statusTone)).toEqual(['ok', 'idle', 'warn', 'warn', 'bad'])
  })

  it('separates the number from the unit in a humantime retention', () => {
    expect(formatRetain('30days')).toBe('30 days')
    expect(formatRetain('1h 30m')).toBe('1 h 30 m')
    expect(formatRetain('2592000')).toBe('30d')
  })

  it('drops a rate-limit message that repeats the "until" line', () => {
    const until = 'rate limited until 2026-11-05T00:41:51.123+00:00'
    expect(credentialErrorText({ status: 'rate_limited', last_error: until })).toBeNull()
    expect(credentialErrorText({ status: 'active', last_error: null })).toBeNull()
    expect(credentialErrorText({ status: 'invalid', last_error: 'bad key' })).toBe('bad key')
    expect(credentialErrorText({ status: 'invalid', last_error: until })).not.toMatch(/T\d\d:/)
  })
})
