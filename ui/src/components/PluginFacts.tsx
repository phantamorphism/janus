import { Badge } from '@/components/Badge'
import { Time } from '@/components/Time'
import type { PluginMode, PluginStatus } from '@/api/types'
import { words } from '@/lib/format'
import { healthTone } from '@/lib/tone'

const modeLabel: Record<PluginMode, string> = {
  none: 'No plugin (pass-through to the default chain)',
  linked: 'Linked into this daemon build',
  unix: 'gRPC over a unix socket',
  tls: 'gRPC over mutual TLS',
  plaintext: 'gRPC over plaintext TCP',
}

/** Days until `ts`, for certificate expiry warnings. */
function daysUntil(ts: string | null | undefined): number | null {
  if (!ts) return null
  const t = new Date(ts).getTime()
  if (Number.isNaN(t)) return null
  return Math.floor((t - Date.now()) / 86_400_000)
}

export function CertExpiry({ label, value }: { label: string; value: string | null | undefined }) {
  const days = daysUntil(value)
  return (
    <div>
      <dt className="text-xs text-muted">{label}</dt>
      <dd className="mt-0.5 text-sm">
        {value ? (
          <>
            <Time value={value} />
            {days !== null && days < 0 && (
              <span className="ml-2">
                <Badge tone="bad">expired</Badge>
              </span>
            )}
            {days !== null && days >= 0 && days < 14 && (
              <span className="ml-2">
                <Badge tone="warn">expires in {days} d</Badge>
              </span>
            )}
          </>
        ) : (
          <span className="text-muted">not seen yet</span>
        )}
      </dd>
    </div>
  )
}

export function PluginFacts({ plugin }: { plugin: PluginStatus }) {
  return (
    <dl className="grid grid-cols-1 gap-x-8 gap-y-4 sm:grid-cols-2">
      <div>
        <dt className="text-xs text-muted">Health</dt>
        <dd className="mt-0.5">
          <Badge tone={healthTone(plugin.health)}>{words(plugin.health)}</Badge>
        </dd>
      </div>
      <div>
        <dt className="text-xs text-muted">Mode</dt>
        <dd className="mt-0.5 text-sm">
          <span className="mono">{plugin.mode}</span>
          <span className="ml-2 text-muted">{modeLabel[plugin.mode] ?? ''}</span>
        </dd>
      </div>
      <div>
        <dt className="text-xs text-muted">Endpoint</dt>
        <dd className="mono mt-0.5 break-all">{plugin.endpoint ?? <span className="text-muted">none</span>}</dd>
      </div>
      <div>
        <dt className="text-xs text-muted">Last successful call</dt>
        <dd className="mt-0.5 text-sm">
          <Time value={plugin.last_ok_at} empty="none yet" />
        </dd>
      </div>
      {plugin.mode === 'tls' && (
        <>
          <CertExpiry label="Daemon client certificate expires" value={plugin.client_cert_expires_at} />
          <CertExpiry label="Plugin server certificate expires" value={plugin.server_cert_expires_at} />
        </>
      )}
      {plugin.last_error && (
        <div className="sm:col-span-2">
          <dt className="text-xs text-muted">Last error</dt>
          <dd className="mono mt-0.5 break-words text-bad">{plugin.last_error}</dd>
        </div>
      )}
    </dl>
  )
}

