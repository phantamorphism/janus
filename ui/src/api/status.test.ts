import { describe, expect, it } from 'vitest'
import { parsePlugin, parseStatus } from './status'
import { ApiError } from './client'
import { plugin, status } from '@/test/fixtures'

describe('parseStatus', () => {
  it('reads the daemon status document exactly', () => {
    const s = parseStatus(status())
    expect(s.listeners).toEqual({
      admin_socket: '/var/lib/janus/admin.sock',
      admin_tcp: null,
      ui: { address: '127.0.0.1:8090', tls: false, url: 'http://127.0.0.1:8090' },
      data_plane: '127.0.0.1:8080',
    })
    expect(s.upstreams[0]).toEqual({
      name: 'openai',
      path_prefix: '/openai',
      base_url: 'https://api.openai.com/',
      binding: 'hmac',
      routes: [{ path: '/openai/v1/embeddings', binding: 'jwt' }],
    })
    expect(s.request_log).toEqual({ enabled: true, retain: '30days', retain_seconds: 2_592_000, rows: 5321, dropped_records: 0 })
    expect(s.data_plane).toEqual({ refused_connections: 0 })
    expect(s.credentials.by_status.invalid).toBe(1)
    expect(s.plugin.dropped_events).toBe(0)
  })

  it('accepts a null upstream binding and a plugin without certificate fields', () => {
    const s = parseStatus(
      status({
        upstreams: [{ name: 'openrouter', path_prefix: '/or', base_url: 'https://x/', binding: null, routes: [] }],
        plugin: { mode: 'none', endpoint: null, health: 'unknown', last_error: null, last_ok_at: null, dropped_events: 3 },
      }),
    )
    expect(s.upstreams[0].binding).toBeNull()
    expect(s.plugin.client_cert_expires_at).toBeUndefined()
    expect(s.plugin.dropped_events).toBe(3)
  })

  it.each([
    ['a missing listeners block', { listeners: undefined }],
    ['listeners given as a list', { listeners: [] }],
    ['a request_log without rows', { request_log: { enabled: true, retain: '1day', retain_seconds: 1, dropped_records: 0 } }],
    ['a request_log without dropped_records', { request_log: { enabled: true, retain: '1day', retain_seconds: 1, rows: 1 } }],
    ['a missing data_plane', { data_plane: undefined }],
    ['a data_plane without refused_connections', { data_plane: {} }],
    ['missing credentials', { credentials: undefined }],
    ['a plugin without dropped_events', { plugin: { ...plugin(), dropped_events: undefined } }],
  ])('rejects %s instead of guessing', (_name, over) => {
    expect(() => parseStatus(status(over))).toThrow(ApiError)
  })

  it('rejects a non-object document', () => {
    expect(() => parseStatus([])).toThrow(/unexpected response shape/)
    expect(() => parsePlugin(null)).toThrow(/unexpected response shape/)
  })
})
