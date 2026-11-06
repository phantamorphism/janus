import { ApiError } from './client'
import type {
  CredentialFilter,
  CredentialStatus,
  DaemonStatus,
  Listeners,
  PluginStatus,
  RouteSummary,
  TcpListenerInfo,
  UpstreamSummary,
} from './types'
import { CREDENTIAL_STATUSES, UPSTREAMS } from './types'

const STRATEGIES: readonly string[] = ['ordered', 'round_robin', 'least_recently_used', 'sticky']

// Strict readers for `GET /v1/status` and `GET /v1/plugin`, whose shapes are
// fixed by the daemon (`admin/status.rs`, janus-core `PluginStatus`). A
// document that does not match is a contract break and surfaces as an error
// rather than being coerced.

type Json = Record<string, unknown>

function bad(what: string): never {
  throw new ApiError(200, 'unexpected_response', `unexpected response shape: ${what}`)
}

function obj(v: unknown, at: string): Json {
  if (typeof v !== 'object' || v === null || Array.isArray(v)) bad(`${at} is not an object`)
  return v as Json
}

function str(o: Json, k: string, at: string): string {
  const v = o[k]
  if (typeof v !== 'string') bad(`${at}.${k} is not a string`)
  return v
}

function strOrNull(o: Json, k: string, at: string): string | null {
  const v = o[k]
  if (v === null || v === undefined) return null
  if (typeof v !== 'string') bad(`${at}.${k} is not a string or null`)
  return v
}

function num(o: Json, k: string, at: string): number {
  const v = o[k]
  if (typeof v !== 'number') bad(`${at}.${k} is not a number`)
  return v
}

function bool(o: Json, k: string, at: string): boolean {
  const v = o[k]
  if (typeof v !== 'boolean') bad(`${at}.${k} is not a boolean`)
  return v
}

function list(o: Json, k: string, at: string): unknown[] {
  const v = o[k]
  if (!Array.isArray(v)) bad(`${at}.${k} is not a list`)
  return v
}

function tcp(v: unknown, at: string): TcpListenerInfo | null {
  if (v === null) return null
  const o = obj(v, at)
  return { address: str(o, 'address', at), tls: bool(o, 'tls', at), url: str(o, 'url', at) }
}

function listeners(v: unknown): Listeners {
  const o = obj(v, 'listeners')
  return {
    admin_socket: str(o, 'admin_socket', 'listeners'),
    admin_tcp: tcp(o.admin_tcp, 'listeners.admin_tcp'),
    ui: tcp(o.ui, 'listeners.ui'),
    data_plane: str(o, 'data_plane', 'listeners'),
  }
}

function strings(o: Json, k: string, at: string): string[] {
  return list(o, k, at).map((v, i) => {
    if (typeof v !== 'string') bad(`${at}.${k}[${i}] is not a string`)
    return v
  })
}

function oneOf<T extends string>(o: Json, k: string, at: string, allowed: readonly string[]): T {
  const v = str(o, k, at)
  if (!allowed.includes(v)) bad(`${at}.${k} is ${JSON.stringify(v)}`)
  return v as T
}

function filter(v: unknown, at: string): CredentialFilter | null {
  if (v === null) return null
  const o = obj(v, at)
  const f: CredentialFilter = {}
  if (o.kind !== undefined) f.kind = oneOf(o, 'kind', at, ['api_key', 'oauth'])
  if (o.refs !== undefined) f.refs = strings(o, 'refs', at)
  if (o.tags !== undefined) f.tags = strings(o, 'tags', at)
  return f
}

function upstream(v: unknown, i: number): UpstreamSummary {
  const at = `upstreams[${i}]`
  const o = obj(v, at)
  const routes: RouteSummary[] = list(o, 'routes', at).map((r, j) => {
    const ro = obj(r, `${at}.routes[${j}]`)
    return { path: str(ro, 'path', `${at}.routes[${j}]`), binding: str(ro, 'binding', `${at}.routes[${j}]`) }
  })
  return {
    name: str(o, 'name', at),
    implementation: oneOf(o, 'implementation', at, UPSTREAMS),
    strategy: oneOf(o, 'strategy', at, STRATEGIES),
    failover: bool(o, 'failover', at),
    credentials: filter(o.credentials, `${at}.credentials`),
    eligible: strings(o, 'eligible', at),
    path_prefix: str(o, 'path_prefix', at),
    base_url: str(o, 'base_url', at),
    binding: strOrNull(o, 'binding', at),
    routes,
  }
}

/** `GET /v1/plugin`, and the `plugin` object of `GET /v1/status`. */
export function parsePlugin(v: unknown, at = 'plugin'): PluginStatus {
  const o = obj(v, at)
  const optional = (k: string) => strOrNull(o, k, at)
  const status: PluginStatus = {
    mode: str(o, 'mode', at) as PluginStatus['mode'],
    endpoint: optional('endpoint'),
    health: str(o, 'health', at) as PluginStatus['health'],
    last_error: optional('last_error'),
    last_ok_at: optional('last_ok_at'),
    dropped_events: num(o, 'dropped_events', at),
  }
  // TLS transport only: absent from the JSON otherwise.
  if (o.client_cert_expires_at !== undefined) status.client_cert_expires_at = optional('client_cert_expires_at')
  if (o.server_cert_expires_at !== undefined) status.server_cert_expires_at = optional('server_cert_expires_at')
  return status
}

export function parseStatus(raw: unknown): DaemonStatus {
  const o = obj(raw, 'status')
  const log = obj(o.request_log, 'request_log')
  const pool = obj(o.credentials, 'credentials')
  const byStatus = obj(pool.by_status, 'credentials.by_status')
  const by_status = Object.fromEntries(
    CREDENTIAL_STATUSES.map((s) => [s, num(byStatus, s, 'credentials.by_status')]),
  ) as Record<CredentialStatus, number>
  return {
    version: str(o, 'version', 'status'),
    build: str(o, 'build', 'status'),
    started_at: str(o, 'started_at', 'status'),
    listeners: listeners(o.listeners),
    upstreams: list(o, 'upstreams', 'status').map(upstream),
    data_plane: {
      refused_connections: num(obj(o.data_plane, 'data_plane'), 'refused_connections', 'data_plane'),
    },
    request_log: {
      enabled: bool(log, 'enabled', 'request_log'),
      retain: str(log, 'retain', 'request_log'),
      retain_seconds: num(log, 'retain_seconds', 'request_log'),
      rows: num(log, 'rows', 'request_log'),
      dropped_records: num(log, 'dropped_records', 'request_log'),
    },
    credentials: { total: num(pool, 'total', 'credentials'), by_status },
    plugin: parsePlugin(o.plugin),
  }
}
