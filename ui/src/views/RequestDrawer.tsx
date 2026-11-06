import type { ReactNode } from 'react'
import { useApi } from '@/api/context'
import type { RequestRecord } from '@/api/types'
import { Badge, StatusBadge } from '@/components/Badge'
import { Drawer } from '@/components/Drawer'
import { ErrorNotice } from '@/components/States'
import { Time } from '@/components/Time'
import { formatDuration, words } from '@/lib/format'
import { outcomeQualifier } from '@/lib/outcome'
import { outcomeTone } from '@/lib/tone'
import { recordUpstream } from '@/lib/upstream'
import { useResource } from '@/lib/useResource'

function Field({ label, children, mono }: { label: string; children: ReactNode; mono?: boolean }) {
  return (
    <div className="grid grid-cols-[9.5rem_1fr] gap-3 border-b border-line py-2 last:border-0">
      <dt className="text-sm text-muted">{label}</dt>
      <dd className={`min-w-0 break-words ${mono ? 'mono' : 'text-sm'}`}>{children}</dd>
    </div>
  )
}

const none = <span className="text-muted">—</span>

/** One request-log record, fetched fresh from `GET /v1/requests/{id}`. */
export function RequestDrawer({ initial, onClose }: { initial: RequestRecord; onClose: () => void }) {
  const api = useApi()
  const fresh = useResource((signal) => api.request(initial.request_id, signal), `request:${initial.request_id}`)
  const r = fresh.data ?? initial
  const qualifier = outcomeQualifier(r)
  return (
    <Drawer title="Request" onClose={onClose}>
      {fresh.error !== undefined && (
        <ErrorNotice error={fresh.error} what="the latest copy of this record" onRetry={fresh.reload} />
      )}
      <dl aria-label="Request record">
        <Field label="Request id" mono>
          {r.request_id}
        </Field>
        <Field label="Received">
          <Time value={r.received_at} />
        </Field>
        <Field label="Duration">{formatDuration(r.duration_ms)}</Field>
        <Field label="Outcome">
          <Badge tone={outcomeTone(r.outcome)}>{words(r.outcome)}</Badge>
          {qualifier && <span className="ml-2">{qualifier}</span>}
        </Field>
        <Field label="Upstream">{recordUpstream(r) ?? none}</Field>
        <Field label="Method and path" mono>
          {r.method} {r.path}
        </Field>
        <Field label="Route" mono>
          {r.route ?? none}
        </Field>
        <Field label="Binding" mono>
          {r.binding ?? none}
        </Field>
        <Field label="Caller identity" mono>
          {r.caller_identity || none}
        </Field>
        {r.claimed_identity && (
          <Field label="Claimed identity" mono>
            {r.claimed_identity}
            <div className="font-sans text-xs text-muted">
              Decoded without verification from a rejected request. Logged only; it authorized nothing.
            </div>
          </Field>
        )}
        <Field label="Correlation id" mono>
          {r.correlation_id ?? none}
        </Field>
        <Field label="Credential" mono>
          {r.credential_ref ?? none}
        </Field>
        {r.attempts.length > 0 && (
          <Field label="Failed over from">
            <ol aria-label="Failed over from" className="space-y-1">
              {r.attempts.map((a, i) => (
                <li key={i} className="flex flex-wrap items-center gap-2">
                  <span className="mono">{a.credential_ref}</span>
                  <StatusBadge code={a.status} />
                  <span className="text-muted">{words(a.verdict)}</span>
                </li>
              ))}
            </ol>
          </Field>
        )}
        <Field label="Upstream status">
          {r.status === null ? none : <StatusBadge code={r.status} />}
        </Field>
        <Field label="Error" mono>
          {r.error ?? none}
        </Field>
        <Field label="Usage">
          {r.usage === null || r.usage === undefined ? (
            none
          ) : (
            <pre className="mono overflow-x-auto rounded bg-sunken p-3">{JSON.stringify(r.usage, null, 2)}</pre>
          )}
        </Field>
      </dl>
    </Drawer>
  )
}
