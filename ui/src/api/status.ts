import type {
  DaemonStatus,
  ListenerInfo,
  PluginStatus,
  RequestLogInfo,
  UpstreamInfo,
} from './types'

// `docs/admin-surface#status-endpoint` fixes which facts `/v1/status`
// carries, and the plugin object exactly, but not how listeners, upstreams
// and the request log are nested. This reads the plausible encodings (a map
// or a list of listeners; a binding given as a method string or an object
// with `method`) into one shape, and never invents a value that is absent.

type Json = Record<string, unknown>

function isObject(v: unknown): v is Json {
  return typeof v === 'object' && v !== null && !Array.isArray(v)
}

function str(v: unknown): string | null {
  if (typeof v === 'string') return v
  if (typeof v === 'number' || typeof v === 'boolean') return String(v)
  return null
}

function firstString(o: Json, keys: string[]): string | null {
  for (const k of keys) {
    const s = str(o[k])
    if (s !== null && s !== '') return s
  }
  return null
}

function listenerFrom(name: string, v: unknown): ListenerInfo | null {
  if (v === null || v === undefined || v === false) return null
  if (typeof v === 'string') return { name, address: v, tls: null }
  if (isObject(v)) {
    const address = firstString(v, ['address', 'bind', 'addr', 'url', 'path', 'socket'])
    if (address === null) return null
    const tls = typeof v.tls === 'boolean' ? v.tls : isObject(v.tls) ? true : null
    return { name: firstString(v, ['name', 'kind', 'listener']) ?? name, address, tls }
  }
  return null
}

function normalizeListeners(raw: unknown): ListenerInfo[] {
  if (Array.isArray(raw)) {
    return raw
      .map((item, i) => listenerFrom(`listener ${i + 1}`, item))
      .filter((l): l is ListenerInfo => l !== null)
  }
  if (isObject(raw)) {
    return Object.entries(raw)
      .map(([name, v]) => listenerFrom(name, v))
      .filter((l): l is ListenerInfo => l !== null)
  }
  return []
}

function bindingOf(o: Json): string | null {
  for (const key of ['binding', 'identity', 'method']) {
    const v = o[key]
    if (typeof v === 'string') return v
    if (isObject(v)) {
      const m = firstString(v, ['method'])
      if (m !== null) return m
    }
  }
  return null
}

function upstreamFrom(name: string | null, v: unknown): UpstreamInfo | null {
  if (!isObject(v)) return null
  const n = firstString(v, ['name', 'upstream', 'kind']) ?? name
  if (n === null) return null
  return { name: n, path_prefix: firstString(v, ['path_prefix', 'prefix']), binding: bindingOf(v) }
}

function normalizeUpstreams(raw: unknown): UpstreamInfo[] {
  if (Array.isArray(raw)) {
    return raw.map((v) => upstreamFrom(null, v)).filter((u): u is UpstreamInfo => u !== null)
  }
  if (isObject(raw)) {
    return Object.entries(raw)
      .map(([name, v]) => upstreamFrom(name, v))
      .filter((u): u is UpstreamInfo => u !== null)
  }
  return []
}

function normalizeRequestLog(raw: unknown): RequestLogInfo | null {
  if (!isObject(raw)) return null
  const rows = [raw.rows, raw.size, raw.count].find((v) => typeof v === 'number') as
    | number
    | undefined
  return {
    enabled: raw.enabled !== false,
    retain: str(raw.retain),
    rows: rows ?? null,
  }
}

export function normalizeStatus(raw: unknown): DaemonStatus {
  const o = isObject(raw) ? raw : {}
  const plugin = (isObject(o.plugin) ? o.plugin : { mode: 'none', health: 'unknown' }) as Json
  return {
    version: str(o.version) ?? 'unknown',
    build: str(o.build) ?? 'unknown',
    started_at: str(o.started_at),
    listeners: normalizeListeners(o.listeners),
    upstreams: normalizeUpstreams(o.upstreams),
    request_log: normalizeRequestLog(o.request_log),
    plugin: {
      mode: (str(plugin.mode) ?? 'none') as PluginStatus['mode'],
      endpoint: str(plugin.endpoint),
      health: (str(plugin.health) ?? 'unknown') as PluginStatus['health'],
      last_error: str(plugin.last_error),
      last_ok_at: str(plugin.last_ok_at),
      client_cert_expires_at: str(plugin.client_cert_expires_at),
      server_cert_expires_at: str(plugin.server_cert_expires_at),
    },
  }
}
