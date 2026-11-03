// JSON shapes of the admin API (`docs/admin-surface#endpoints`), mirroring
// the serde types the daemon emits:
//
// - CredentialMetadata: janus-core contract/credential.rs
// - RequestRecord + Outcome: janus-core contract/request_log.rs
//   (outcome is an internally tagged enum flattened into the record)
// - IdentityRecord: janus-core contract/registry.rs
// - PluginStatus: janus-core contract/plugin.rs
// - ChannelCheck: janus-daemon plugin/check.rs
//
// Timestamps are RFC 3339 strings (chrono's serde form).

export type UpstreamKind = 'anthropic' | 'openai' | 'openrouter'
export const UPSTREAMS: readonly UpstreamKind[] = ['anthropic', 'openai', 'openrouter']

export type Timestamp = string

// ---- errors ----------------------------------------------------------------

export interface ErrorBody {
  error: { code: string; message: string }
}

// ---- credentials -----------------------------------------------------------

export type CredentialKind = 'api_key' | 'oauth'
export type CredentialSource = 'pool' | 'env' | 'file'
export type CredentialStatus = 'active' | 'invalid' | 'disabled' | 'rate_limited'
export const CREDENTIAL_STATUSES: readonly CredentialStatus[] = [
  'active',
  'rate_limited',
  'invalid',
  'disabled',
]

export interface CredentialMetadata {
  ref: string
  upstream: UpstreamKind
  kind: CredentialKind
  label: string | null
  source: CredentialSource
  status: CredentialStatus
  rate_limited_until: Timestamp | null
  expires_at: Timestamp | null
  last_used_at: Timestamp | null
  request_count: number
  error_count: number
  last_error: string | null
}

// ---- identities ------------------------------------------------------------

export interface IdentityRecord {
  id: string
  identity: string
  claims: Record<string, string>
  label: string | null
  created_at: Timestamp
  expires_at: Timestamp | null
  revoked_at: Timestamp | null
  last_seen_at: Timestamp | null
}

export interface IdentityFilter {
  identity?: string
  include_revoked?: boolean
  include_expired?: boolean
}

// ---- request log -----------------------------------------------------------

export type OutcomeName =
  | 'forwarded'
  | 'rejected_identity'
  | 'denied'
  | 'plugin_failed'
  | 'credential_unavailable'
  | 'upstream_error'
  | 'abandoned'

export const OUTCOMES: readonly OutcomeName[] = [
  'forwarded',
  'rejected_identity',
  'denied',
  'plugin_failed',
  'credential_unavailable',
  'upstream_error',
  'abandoned',
]

export type RejectionKind = 'no_material' | 'invalid' | 'wrong_namespace' | 'expired' | 'revoked'
/** On the record the failure class keeps its `plugin_` prefix. */
export type PluginFailureClass =
  | 'plugin_unreachable'
  | 'plugin_timeout'
  | 'plugin_tls_error'
  | 'plugin_no_client_auth'
  | 'plugin_rejected_client_cert'
  | 'plugin_error'
export type AbandonStage = 'resolve' | 'credential' | 'rewrite' | 'forward'

export type Outcome =
  | { outcome: 'forwarded' }
  | { outcome: 'rejected_identity'; kind: RejectionKind }
  | { outcome: 'denied'; reason: string }
  | { outcome: 'plugin_failed'; class: PluginFailureClass }
  | { outcome: 'credential_unavailable' }
  | { outcome: 'upstream_error' }
  | { outcome: 'abandoned'; stage: AbandonStage }

export type RequestRecord = Outcome & {
  request_id: string
  received_at: Timestamp
  duration_ms: number
  upstream: UpstreamKind | null
  route: string | null
  method: string
  path: string
  /** `hmac`, `jwt`, `opaque`, `trusted_header`, `none` or `custom:<name>`. */
  binding: string | null
  caller_identity: string
  claimed_identity: string | null
  correlation_id: string | null
  credential_ref: string | null
  status: number | null
  usage: unknown
  error: string | null
}

export interface RequestFilter {
  identity?: string
  upstream?: UpstreamKind
  outcome?: OutcomeName
  credential?: string
  /** RFC 3339. */
  since?: string
  /** RFC 3339. */
  until?: string
}

export interface RequestQuery extends RequestFilter {
  limit?: number
  before?: string
}

export interface RequestPage {
  records: RequestRecord[]
  /** Cursor for the next (older) page; null on the last one. */
  next: string | null
  /** `request_log.enabled = false`: empty by configuration. */
  disabled: boolean
}

/** `limit` default and cap of `GET /v1/requests`. */
export const DEFAULT_LIMIT = 100
export const MAX_LIMIT = 1000

// ---- plugin ----------------------------------------------------------------

export type PluginMode = 'none' | 'linked' | 'unix' | 'tls' | 'plaintext'
export type PluginHealth =
  | 'unknown'
  | 'ok'
  | 'unreachable'
  | 'timeout'
  | 'tls_error'
  | 'no_client_auth'
  | 'rejected_client_cert'
  | 'error'

export interface PluginStatus {
  mode: PluginMode
  endpoint: string | null
  health: PluginHealth
  last_error: string | null
  last_ok_at: Timestamp | null
  client_cert_expires_at?: Timestamp | null
  server_cert_expires_at?: Timestamp | null
}

export type CheckStepKind =
  | 'path_permissions'
  | 'connect'
  | 'handshake'
  | 'throwaway_cert_rejected'
  | 'no_cert_rejected'

export interface WalkedDir {
  path: string
  uid: number
  /** Permission bits (`mode & 0o7777`) as a decimal number. */
  mode: number
}

export interface CheckStep {
  step: CheckStepKind
  passed: boolean
  detail: string
  class?: PluginFailureClass
  walk?: { socket: string; dirs: WalkedDir[] }
}

export interface ChannelCheck {
  mode: PluginMode
  endpoint: string | null
  passed: boolean
  steps: CheckStep[]
}

// ---- status ----------------------------------------------------------------

/**
 * `GET /v1/status` as the UI uses it, after `normalizeStatus`. The docs name
 * the fields (version, build, started_at, listeners, upstreams with their
 * binding method, request log size and retention, plugin) but not the
 * nesting of listeners/upstreams/request_log, so those are read tolerantly.
 */
export interface DaemonStatus {
  version: string
  build: string
  started_at: Timestamp | null
  listeners: ListenerInfo[]
  upstreams: UpstreamInfo[]
  request_log: RequestLogInfo | null
  plugin: PluginStatus
}

export interface ListenerInfo {
  name: string
  address: string
  tls: boolean | null
}

export interface UpstreamInfo {
  name: string
  path_prefix: string | null
  binding: string | null
}

export interface RequestLogInfo {
  enabled: boolean
  /** As sent: a humantime string ("30d") or a number of seconds. */
  retain: string | null
  rows: number | null
}
