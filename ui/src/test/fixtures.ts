import type {
  CredentialMetadata,
  IdentityRecord,
  PluginStatus,
  RequestRecord,
} from '@/api/types'

export function credential(over: Partial<CredentialMetadata> = {}): CredentialMetadata {
  return {
    ref: 'team-a',
    upstream: 'anthropic',
    kind: 'oauth',
    label: 'Team A',
    source: 'pool',
    status: 'active',
    rate_limited_until: null,
    expires_at: '2030-01-01T00:00:00Z',
    last_used_at: '2026-11-03T10:00:00Z',
    request_count: 1234,
    error_count: 2,
    last_error: null,
    tags: [],
    ...over,
  }
}

export function identity(over: Partial<IdentityRecord> = {}): IdentityRecord {
  return {
    id: 'idn_01',
    identity: 'sandbox-7',
    claims: { team: 'blue' },
    label: 'CI sandbox',
    created_at: '2026-11-01T09:00:00Z',
    expires_at: null,
    revoked_at: null,
    last_seen_at: '2026-11-03T09:30:00Z',
    ...over,
  }
}

export function record(over: Partial<RequestRecord> = {}): RequestRecord {
  return {
    request_id: 'req-1',
    received_at: '2026-11-03T10:00:00Z',
    duration_ms: 412,
    upstream: 'openai',
    implementation: 'openai',
    route: null,
    method: 'POST',
    path: '/openai/v1/chat/completions',
    binding: 'hmac',
    caller_identity: 'sandbox-7',
    claimed_identity: null,
    correlation_id: 'task-42',
    outcome: 'forwarded',
    credential_ref: 'team-a',
    status: 200,
    usage: { input_tokens: 12, output_tokens: 40 },
    error: null,
    attempts: [],
    ...over,
  } as RequestRecord
}

export function plugin(over: Partial<PluginStatus> = {}): PluginStatus {
  return {
    mode: 'tls',
    endpoint: 'https://policy.internal:7443',
    health: 'ok',
    last_error: null,
    last_ok_at: '2026-11-03T10:00:00Z',
    client_cert_expires_at: '2027-11-03T00:00:00Z',
    server_cert_expires_at: '2027-05-01T00:00:00Z',
    dropped_events: 0,
    ...over,
  }
}

// Envelopes exactly as the daemon serializes them (`admin/credentials.rs`
// `CredentialList`, `admin/identities.rs` `IdentityList`,
// `admin/requests.rs` `Page`). Keep these literal: a wrong key must fail.
export function credentialList(credentials: CredentialMetadata[]) {
  return { credentials }
}

export function identityList(identities: IdentityRecord[]) {
  return { identities }
}

export function requestPage(requests: RequestRecord[], next: string | null = null, disabled = false) {
  return { requests, next, disabled }
}

/** `GET /v1/status` as `admin/status.rs` serializes it (Url adds a trailing slash). */
export function status(over: Record<string, unknown> = {}) {
  return {
    version: '0.1.0',
    build: 'abc1234',
    started_at: '2026-11-03T08:00:00Z',
    listeners: {
      admin_socket: '/var/lib/janus/admin.sock',
      admin_tcp: null,
      ui: { address: '127.0.0.1:8090', tls: false, url: 'http://127.0.0.1:8090' },
      data_plane: '127.0.0.1:8080',
    },
    upstreams: [
      {
        name: 'openai',
        implementation: 'openai',
        strategy: 'ordered',
        failover: false,
        credentials: null,
        eligible: ['oa-main'],
        path_prefix: '/openai',
        base_url: 'https://api.openai.com/',
        binding: 'hmac',
        routes: [{ path: '/openai/v1/embeddings', binding: 'jwt' }],
      },
      {
        name: 'anthropic',
        implementation: 'anthropic',
        strategy: 'ordered',
        failover: false,
        credentials: null,
        eligible: ['team-a'],
        path_prefix: '/anthropic',
        base_url: 'https://api.anthropic.com/',
        binding: 'opaque',
        routes: [],
      },
    ],
    data_plane: { refused_connections: 0 },
    request_log: {
      enabled: true,
      retain: '30days',
      retain_seconds: 2_592_000,
      rows: 5321,
      dropped_records: 0,
    },
    credentials: {
      total: 4,
      by_status: { active: 2, invalid: 1, disabled: 1, rate_limited: 0 },
    },
    plugin: plugin(),
    ...over,
  }
}
