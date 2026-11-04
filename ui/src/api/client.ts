import { parsePlugin, parseStatus } from './status'
import type {
  ChannelCheck,
  CredentialMetadata,
  DaemonStatus,
  IdentityFilter,
  IdentityRecord,
  PluginStatus,
  RequestFilter,
  RequestPage,
  RequestQuery,
  RequestRecord,
  UpstreamKind,
} from './types'

/** A non-2xx answer from the admin API, decoded from its JSON error body. */
export class ApiError extends Error {
  readonly status: number
  readonly code: string

  constructor(status: number, code: string, message: string) {
    super(message)
    this.name = 'ApiError'
    this.status = status
    this.code = code
  }
}

/** The daemon could not be reached at all. */
export class NetworkError extends Error {
  constructor(message: string) {
    super(message)
    this.name = 'NetworkError'
  }
}

export function errorMessage(e: unknown): string {
  if (e instanceof ApiError) return `${e.message} (${e.code}, HTTP ${e.status})`
  if (e instanceof Error) return e.message
  return String(e)
}

type QueryValue = string | number | boolean | undefined | null

export function buildQuery(params: Record<string, QueryValue>): string {
  const q = new URLSearchParams()
  for (const [k, v] of Object.entries(params)) {
    if (v === undefined || v === null || v === '' || v === false) continue
    q.set(k, String(v))
  }
  const s = q.toString()
  return s ? `?${s}` : ''
}

/** The list under `key` of a `{ [key]: [...] }` envelope; any other shape is an error. */
function listOf<T>(json: unknown, key: string): T[] {
  if (typeof json === 'object' && json !== null && !Array.isArray(json)) {
    const inner = (json as Record<string, unknown>)[key]
    if (Array.isArray(inner)) return inner as T[]
  }
  throw new ApiError(200, 'unexpected_response', `unexpected response shape: expected an object with a "${key}" list`)
}

async function decodeError(res: Response): Promise<ApiError> {
  let code = `http_${res.status}`
  let message = res.statusText || `HTTP ${res.status}`
  try {
    const body: unknown = await res.json()
    const err = (body as { error?: { code?: unknown; message?: unknown } })?.error
    if (err && typeof err.code === 'string') code = err.code
    if (err && typeof err.message === 'string') message = err.message
  } catch {
    // A body that is not the JSON error envelope keeps the HTTP status text.
  }
  return new ApiError(res.status, code, message)
}

export interface ApiClientOptions {
  baseUrl: string
  token: string
  /** Called when the daemon answers 401: the token is no longer good. */
  onUnauthorized: () => void
}

interface RequestOptions {
  signal?: AbortSignal
  body?: unknown
}

/**
 * The admin API over `fetch`. Every call carries the admin token as a
 * bearer `Authorization` header; a 401 calls `onUnauthorized` and rejects.
 */
export class ApiClient {
  private readonly opts: ApiClientOptions

  constructor(opts: ApiClientOptions) {
    this.opts = opts
  }

  get token(): string {
    return this.opts.token
  }

  url(path: string): string {
    return `${this.opts.baseUrl}${path}`
  }

  headers(extra: Record<string, string> = {}): Record<string, string> {
    return { Authorization: `Bearer ${this.opts.token}`, ...extra }
  }

  /** Report a 401 seen outside `send` (the live tail). */
  unauthorized(): void {
    this.opts.onUnauthorized()
  }

  private async send(method: string, path: string, o: RequestOptions = {}): Promise<unknown> {
    const headers = this.headers({ Accept: 'application/json' })
    if (o.body !== undefined) headers['Content-Type'] = 'application/json'
    let res: Response
    try {
      res = await fetch(this.url(path), {
        method,
        headers,
        body: o.body === undefined ? undefined : JSON.stringify(o.body),
        signal: o.signal,
        cache: 'no-store',
      })
    } catch (e) {
      if (o.signal?.aborted) throw e
      throw new NetworkError(`Could not reach the daemon at ${this.url(path)}`)
    }
    if (res.status === 401) {
      const err = await decodeError(res)
      this.opts.onUnauthorized()
      throw err
    }
    if (!res.ok) throw await decodeError(res)
    if (res.status === 204) return null
    const text = await res.text()
    return text ? JSON.parse(text) : null
  }

  // ---- status / plugin ----

  async status(signal?: AbortSignal): Promise<DaemonStatus> {
    return parseStatus(await this.send('GET', '/status', { signal }))
  }

  async plugin(signal?: AbortSignal): Promise<PluginStatus> {
    return parsePlugin(await this.send('GET', '/plugin', { signal }))
  }

  async checkPlugin(signal?: AbortSignal): Promise<ChannelCheck> {
    return (await this.send('POST', '/plugin/check', { signal })) as ChannelCheck
  }

  // ---- credentials ----

  async credentials(upstream?: UpstreamKind, signal?: AbortSignal): Promise<CredentialMetadata[]> {
    const json = await this.send('GET', `/credentials${buildQuery({ upstream })}`, { signal })
    return listOf<CredentialMetadata>(json, 'credentials')
  }

  private credentialAction(ref: string, action: 'enable' | 'disable' | 'refresh') {
    return this.send('POST', `/credentials/${encodeURIComponent(ref)}/${action}`)
  }

  async enableCredential(ref: string): Promise<void> {
    await this.credentialAction(ref, 'enable')
  }

  async disableCredential(ref: string): Promise<void> {
    await this.credentialAction(ref, 'disable')
  }

  async refreshCredential(ref: string): Promise<void> {
    await this.credentialAction(ref, 'refresh')
  }

  async deleteCredential(ref: string): Promise<void> {
    await this.send('DELETE', `/credentials/${encodeURIComponent(ref)}`)
  }

  // ---- identities ----

  async identities(filter: IdentityFilter, signal?: AbortSignal): Promise<IdentityRecord[]> {
    const q = buildQuery({
      identity: filter.identity?.trim(),
      include_revoked: filter.include_revoked,
      include_expired: filter.include_expired,
    })
    return listOf<IdentityRecord>(await this.send('GET', `/identities${q}`, { signal }), 'identities')
  }

  async revokeIdentity(id: string): Promise<void> {
    await this.send('POST', `/identities/${encodeURIComponent(id)}/revoke`)
  }

  // ---- requests ----

  async requests(query: RequestQuery, signal?: AbortSignal): Promise<RequestPage> {
    const json = await this.send('GET', `/requests${buildQuery({ ...query })}`, { signal })
    const next = (json as { next?: unknown } | null)?.next
    const disabled = (json as { disabled?: unknown } | null)?.disabled
    if ((next !== null && typeof next !== 'string') || typeof disabled !== 'boolean') {
      throw new ApiError(200, 'unexpected_response', 'unexpected response shape: expected { requests, next, disabled }')
    }
    return { requests: listOf<RequestRecord>(json, 'requests'), next, disabled }
  }

  async request(requestId: string, signal?: AbortSignal): Promise<RequestRecord> {
    return (await this.send('GET', `/requests/${encodeURIComponent(requestId)}`, {
      signal,
    })) as RequestRecord
  }

  /** The live-tail URL; it carries only filters, never the token. */
  streamUrl(filter: Pick<RequestFilter, 'identity' | 'upstream' | 'outcome'>): string {
    return this.url(
      `/requests/stream${buildQuery({
        identity: filter.identity,
        upstream: filter.upstream,
        outcome: filter.outcome,
      })}`,
    )
  }
}
