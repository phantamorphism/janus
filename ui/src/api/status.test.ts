import { describe, expect, it } from 'vitest'
import { normalizeStatus } from './status'
import { status } from '@/test/fixtures'

describe('normalizeStatus', () => {
  it('reads listeners given as a map and upstream bindings given as string or object', () => {
    const s = normalizeStatus(status())
    expect(s.listeners).toEqual([
      { name: 'data_plane', address: '127.0.0.1:8080', tls: null },
      { name: 'admin_socket', address: '/var/lib/janus/admin.sock', tls: null },
      { name: 'ui', address: '127.0.0.1:8090', tls: false },
    ])
    expect(s.upstreams).toEqual([
      { name: 'openai', path_prefix: '/openai', binding: 'hmac' },
      { name: 'anthropic', path_prefix: '/anthropic', binding: 'opaque' },
    ])
    expect(s.request_log).toEqual({ enabled: true, retain: '30d', rows: 5321 })
  })

  it('reads listeners given as a list, omits unbound ones, and never invents absent values', () => {
    const s = normalizeStatus({
      version: '1',
      build: 'b',
      started_at: null,
      listeners: [
        { kind: 'admin_tcp', bind: '127.0.0.1:7000', tls: { cert: '/c' } },
        { kind: 'ui', bind: null },
      ],
      upstreams: { openrouter: { identity: { method: 'jwt' } } },
      plugin: { mode: 'none', endpoint: null, health: 'unknown', last_error: null, last_ok_at: null },
    })
    expect(s.listeners).toEqual([{ name: 'admin_tcp', address: '127.0.0.1:7000', tls: true }])
    expect(s.upstreams).toEqual([{ name: 'openrouter', path_prefix: null, binding: 'jwt' }])
    expect(s.request_log).toBeNull()
    expect(s.plugin.client_cert_expires_at).toBeNull()
  })
})
