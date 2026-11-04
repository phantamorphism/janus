import { useState } from 'react'
import { RefreshCw } from 'lucide-react'
import { useApi } from '@/api/context'
import { CREDENTIAL_STATUSES, MAX_LIMIT, OUTCOMES, type CredentialMetadata, type DaemonStatus, type PoolSummary } from '@/api/types'
import { Badge } from '@/components/Badge'
import { PageHeader, Section } from '@/components/PageHeader'
import { PluginFacts } from '@/components/PluginFacts'
import { ErrorNotice, Loading } from '@/components/States'
import { Time } from '@/components/Time'
import { formatCount, words } from '@/lib/format'
import { loadTraffic, MAX_PAGES, type Traffic } from '@/lib/traffic'
import { credentialTone, outcomeTone } from '@/lib/tone'
import { useResource } from '@/lib/useResource'

const WINDOWS = [
  { label: 'Last 15 minutes', minutes: 15 },
  { label: 'Last hour', minutes: 60 },
  { label: 'Last 24 hours', minutes: 1440 },
] as const

function perMinute(n: number, minutes: number): string {
  const rate = n / minutes
  return rate >= 10 ? rate.toFixed(0) : rate >= 1 ? rate.toFixed(1) : rate.toFixed(2)
}

interface ListenerRow {
  name: string
  address: string
  tls: boolean | null
}

/** The data plane, the admin socket, and the TCP listeners that are bound. */
function listenerRows(l: DaemonStatus['listeners']): ListenerRow[] {
  const rows: ListenerRow[] = [
    { name: 'data_plane', address: l.data_plane, tls: null },
    { name: 'admin_socket', address: l.admin_socket, tls: null },
  ]
  if (l.admin_tcp) rows.push({ name: 'admin_tcp', address: l.admin_tcp.url, tls: l.admin_tcp.tls })
  if (l.ui) rows.push({ name: 'ui', address: l.ui.url, tls: l.ui.tls })
  return rows
}

function Counter({ label, value, hint }: { label: string; value: number; hint: string }) {
  return (
    <div>
      <dt className="text-xs text-muted">{label}</dt>
      <dd className={`tabular-nums ${value > 0 ? 'text-bad' : ''}`} aria-label={label}>
        {formatCount(value)}
        {value > 0 && <span className="ml-2 text-muted">since start: {hint}</span>}
      </dd>
    </div>
  )
}

function CredentialHealth({ creds, pool }: { creds: CredentialMetadata[]; pool: PoolSummary | undefined }) {
  // Counts come from `/v1/status` (`credentials.by_status`); the list only
  // names the ones needing attention.
  const counts = CREDENTIAL_STATUSES.map((s) => ({
    s,
    n: pool ? pool.by_status[s] : creds.filter((c) => c.status === s).length,
  }))
  const attention = creds.filter((c) => c.status !== 'active')
  return (
    <>
      <ul className="flex flex-wrap gap-x-8 gap-y-3" aria-label="Credentials by status">
        {counts.map(({ s, n }) => (
          <li key={s} className="flex items-baseline gap-2">
            <span className="font-serif text-3xl tabular-nums">{n}</span>
            <Badge tone={credentialTone(s)}>{words(s)}</Badge>
          </li>
        ))}
      </ul>
      {creds.length === 0 ? (
        <p className="mt-3 text-sm text-muted">The pool is empty; no upstream traffic can be forwarded.</p>
      ) : attention.length > 0 ? (
        <ul className="mt-4 space-y-1 text-sm" aria-label="Credentials needing attention">
          {attention.map((c) => (
            <li key={c.ref}>
              <span className="mono">{c.ref}</span> <span className="text-muted">({c.upstream})</span>{' '}
              {words(c.status)}
              {c.last_error && <span className="text-bad">: {c.last_error}</span>}
            </li>
          ))}
        </ul>
      ) : (
        <p className="mt-3 text-sm text-muted">Every credential is active.</p>
      )}
    </>
  )
}

function TrafficPanel({ t }: { t: Traffic }) {
  if (t.disabled) {
    return (
      <p className="text-sm text-muted">
        The request log is off (<code className="mono">request_log.enabled = false</code>), so there is
        nothing to count.
      </p>
    )
  }
  const floor = t.truncated ? 'at least ' : ''
  const denied = t.byOutcome.denied
  const rejected = t.byOutcome.rejected_identity
  const share = (n: number) => (t.total ? `${((n / t.total) * 100).toFixed(1)}%` : '0%')
  return (
    <>
      <dl className="grid grid-cols-1 gap-6 sm:grid-cols-3">
        <div>
          <dt className="text-sm text-muted">Requests</dt>
          <dd className="font-serif text-4xl tabular-nums" aria-label="Request count">
            {floor}
            {formatCount(t.total)}
          </dd>
          <dd className="text-sm text-muted">{perMinute(t.total, t.minutes)} per minute</dd>
        </div>
        <div>
          <dt className="text-sm text-muted">Denied by the plugin</dt>
          <dd className="font-serif text-4xl tabular-nums" aria-label="Denied count">
            {floor}
            {formatCount(denied)}
          </dd>
          <dd className="text-sm text-muted">
            {perMinute(denied, t.minutes)} per minute, {share(denied)} of requests
          </dd>
        </div>
        <div>
          <dt className="text-sm text-muted">Rejected at identity</dt>
          <dd className="font-serif text-4xl tabular-nums" aria-label="Rejected count">
            {floor}
            {formatCount(rejected)}
          </dd>
          <dd className="text-sm text-muted">
            {perMinute(rejected, t.minutes)} per minute, {share(rejected)} of requests
          </dd>
        </div>
      </dl>
      <table className="data-table mt-6 max-w-md" aria-label="Requests by outcome">
        <thead>
          <tr>
            <th scope="col">Outcome</th>
            <th scope="col" className="text-right">
              Count
            </th>
          </tr>
        </thead>
        <tbody>
          {OUTCOMES.map((o) => (
            <tr key={o}>
              <td>
                <Badge tone={t.byOutcome[o] ? outcomeTone(o) : 'idle'}>{words(o)}</Badge>
              </td>
              <td className="text-right tabular-nums">{formatCount(t.byOutcome[o])}</td>
            </tr>
          ))}
        </tbody>
      </table>
      {t.truncated && (
        <p className="mt-2 text-xs text-muted">
          More than {formatCount(MAX_PAGES * MAX_LIMIT)} requests in this window; counts are a floor. Choose a
          shorter window for exact numbers.
        </p>
      )}
    </>
  )
}

export function Overview() {
  const api = useApi()
  const [minutes, setMinutes] = useState<number>(60)
  const status = useResource((signal) => api.status(signal), 'status')
  const creds = useResource((signal) => api.credentials(undefined, signal), 'credentials')
  const traffic = useResource((signal) => loadTraffic(api, minutes, signal), `traffic:${minutes}`)

  function reloadAll() {
    status.reload()
    creds.reload()
    traffic.reload()
  }

  const s = status.data
  return (
    <>
      <PageHeader
        title="Overview"
        lede="What this Janus daemon is, where it listens, and how its traffic and credentials are doing."
        actions={
          <button
            type="button"
            className="btn"
            onClick={reloadAll}
            disabled={status.loading || creds.loading || traffic.loading}
          >
            <RefreshCw size={14} aria-hidden="true" /> Reload
          </button>
        }
      />

      {status.error !== undefined && <ErrorNotice error={status.error} what="daemon status" onRetry={status.reload} />}
      {s === undefined ? (
        status.loading && <Loading what="daemon status" />
      ) : (
        <>
          <Section title="Daemon">
            <dl className="grid grid-cols-1 gap-x-8 gap-y-3 sm:grid-cols-3">
              <div>
                <dt className="text-xs text-muted">Version</dt>
                <dd className="mono">{s.version}</dd>
              </div>
              <div>
                <dt className="text-xs text-muted">Build</dt>
                <dd className="mono break-all">{s.build}</dd>
              </div>
              <div>
                <dt className="text-xs text-muted">Started</dt>
                <dd className="text-sm">
                  <Time value={s.started_at} />
                </dd>
              </div>
            </dl>
          </Section>

          <div className="grid grid-cols-1 gap-x-10 lg:grid-cols-2">
            <Section title="Listeners">
              <table className="data-table" aria-label="Listeners">
                <thead>
                  <tr>
                    <th scope="col">Listener</th>
                    <th scope="col">Address</th>
                    <th scope="col">TLS</th>
                  </tr>
                </thead>
                <tbody>
                  {listenerRows(s.listeners).map((l) => (
                    <tr key={l.name}>
                      <td>{words(l.name)}</td>
                      <td className="mono break-all">{l.address}</td>
                      <td>{l.tls === null ? <span className="text-muted">—</span> : l.tls ? 'yes' : 'no'}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </Section>
            <Section title="Upstreams">
              {s.upstreams.length === 0 ? (
                <p className="text-sm text-muted">No upstreams are configured.</p>
              ) : (
                <table className="data-table" aria-label="Upstreams">
                  <thead>
                    <tr>
                      <th scope="col">Upstream</th>
                      <th scope="col">Path prefix</th>
                      <th scope="col">Base URL</th>
                      <th scope="col">Identity binding</th>
                      <th scope="col">Routes</th>
                    </tr>
                  </thead>
                  <tbody>
                    {s.upstreams.map((u) => (
                      <tr key={u.name}>
                        <td>{u.name}</td>
                        <td className="mono">{u.path_prefix}</td>
                        <td className="mono break-all">{u.base_url}</td>
                        <td className="mono">{u.binding ?? <span className="text-muted">none</span>}</td>
                        <td className="mono">
                          {u.routes.length === 0 ? (
                            <span className="text-muted">—</span>
                          ) : (
                            <ul>
                              {u.routes.map((r) => (
                                <li key={r.path}>
                                  {r.path} <span className="text-muted">→</span> {r.binding}
                                </li>
                              ))}
                            </ul>
                          )}
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              )}
            </Section>
          </div>

          <Section title="Plugin">
            <PluginFacts plugin={s.plugin} />
          </Section>
        </>
      )}

      <Section title="Credential health">
        {creds.error !== undefined && <ErrorNotice error={creds.error} what="credentials" onRetry={creds.reload} />}
        {creds.data === undefined ? creds.loading && <Loading what="credentials" /> : <CredentialHealth creds={creds.data} pool={s?.credentials} />}
      </Section>

      <Section
        title="Traffic"
        aside={
          <label className="flex items-center gap-2 text-sm">
            <span className="text-muted">Window</span>
            <select className="field" value={minutes} onChange={(e) => setMinutes(Number(e.target.value))}>
              {WINDOWS.map((w) => (
                <option key={w.minutes} value={w.minutes}>
                  {w.label}
                </option>
              ))}
            </select>
          </label>
        }
      >
        {traffic.error !== undefined && (
          <ErrorNotice error={traffic.error} what="request counts" onRetry={traffic.reload} />
        )}
        {traffic.data === undefined ? traffic.loading && <Loading what="request counts" /> : <TrafficPanel t={traffic.data} />}
        {s && (
          <p className="mt-4 text-xs text-muted">
            Request log {s.request_log.enabled ? 'on' : 'off'}, {formatCount(s.request_log.rows)} records stored,
            kept for {s.request_log.retain}.
          </p>
        )}
        {s && (
          <dl className="mt-3 flex flex-wrap gap-x-8 gap-y-2 text-sm">
            <Counter label="Dropped log records" value={s.request_log.dropped_records} hint="the writer's queue was full; those requests are missing from the log" />
            <Counter label="Refused connections" value={s.data_plane.refused_connections} hint="closed unanswered for being over data_plane.max_connections" />
          </dl>
        )}
      </Section>
    </>
  )
}
