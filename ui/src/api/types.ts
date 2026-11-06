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

/** An upstream implementation. An `[[upstream]]` instance has a name of its own. */
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
  /** The implementation the credential belongs to. */
  upstream: UpstreamKind
  kind: CredentialKind
  label: string | null
  /** Selection tags an instance's credential filter can match. */
  tags: string[]
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

export type Verdict = 'credential_invalid' | 'rate_limited' | 'account_error'

/** A credential the upstream rejected before the request failed over to the next. */
export interface FailedAttempt {
  credential_ref: string
  status: number
  verdict: Verdict
}

export type RequestRecord = Outcome & {
  request_id: string
  received_at: Timestamp
  duration_ms: number
  /** The `[[upstream]]` instance name; null when no `path_prefix` matched. */
  upstream: string | null
  /** The instance's implementation; null exactly when `upstream` is. */
  implementation: UpstreamKind | null
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
  /** Credentials failed over from before `credential_ref`, in order. */
  attempts: FailedAttempt[]
}

export interface RequestFilter {
  identity?: string
  /** An instance name. */
  upstream?: string
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
  requests: RequestRecord[]
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
  /** `Notify` events dropped since start (queue full); the operator's only sign of lost settlement. */
  dropped_events: number
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

/** `GET /v1/status` (daemon `admin/status.rs`), exactly as served. */
export interface DaemonStatus {
  version: string
  build: string
  started_at: Timestamp
  listeners: Listeners
  upstreams: UpstreamSummary[]
  data_plane: DataPlaneSummary
  request_log: RequestLogSummary
  credentials: PoolSummary
  plugin: PluginStatus
}

export interface Listeners {
  admin_socket: string
  admin_tcp: TcpListenerInfo | null
  ui: TcpListenerInfo | null
  data_plane: string
}

export interface TcpListenerInfo {
  address: string
  tls: boolean
  url: string
}

export interface RouteSummary {
  path: string
  binding: string
}

export type Strategy = 'ordered' | 'round_robin' | 'least_recently_used' | 'sticky'

/** `[upstream.credentials]`: every set field must hold; `tags` holds on any one. */
export interface CredentialFilter {
  kind?: CredentialKind
  refs?: string[]
  tags?: string[]
}

export interface UpstreamSummary {
  /** The instance name. */
  name: string
  implementation: UpstreamKind
  strategy: Strategy
  failover: boolean
  /** Null: the instance admits every credential of its implementation. */
  credentials: CredentialFilter | null
  /** Refs the filter admits now, in chain order, whatever their status. */
  eligible: string[]
  path_prefix: string
  base_url: string
  /** The upstream-wide binding method; null when none resolves. */
  binding: string | null
  routes: RouteSummary[]
}

export interface DataPlaneSummary {
  /** Connections closed unanswered since start for being over `data_plane.max_connections`. */
  refused_connections: number
}

export interface RequestLogSummary {
  enabled: boolean
  /** Human-readable, e.g. `30days`. */
  retain: string
  retain_seconds: number
  rows: number
  /** Records dropped since start because the writer's queue was full. */
  dropped_records: number
}

export interface PoolSummary {
  total: number
  by_status: Record<CredentialStatus, number>
}
