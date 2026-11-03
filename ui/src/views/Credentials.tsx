import { useState } from 'react'
import { RefreshCw } from 'lucide-react'
import { errorMessage } from '@/api/client'
import { useApi } from '@/api/context'
import { UPSTREAMS, type CredentialMetadata, type UpstreamKind } from '@/api/types'
import { Badge } from '@/components/Badge'
import { ConfirmAction } from '@/components/ConfirmAction'
import { PageHeader } from '@/components/PageHeader'
import { ActionNotice, Empty, ErrorNotice, Loading } from '@/components/States'
import { Time } from '@/components/Time'
import { formatCount, words } from '@/lib/format'
import { credentialTone } from '@/lib/tone'
import { useResource } from '@/lib/useResource'

type Action = 'enable' | 'disable' | 'refresh' | 'remove'

const done: Record<Action, string> = {
  enable: 'Enabled',
  disable: 'Disabled',
  refresh: 'Refreshed',
  remove: 'Removed',
}

const kindLabel: Record<CredentialMetadata['kind'], string> = {
  api_key: 'API key',
  oauth: 'OAuth',
}

const sourceLabel: Record<CredentialMetadata['source'], string> = {
  pool: 'pool slot',
  env: 'config (env)',
  file: 'config (file)',
}

export function Credentials() {
  const api = useApi()
  const [upstream, setUpstream] = useState<UpstreamKind | ''>('')
  const list = useResource(
    (signal) => api.credentials(upstream || undefined, signal),
    `credentials:${upstream}`,
  )
  const [busy, setBusy] = useState<string | null>(null)
  const [notice, setNotice] = useState<{ ok: boolean; text: string } | null>(null)

  async function act(c: CredentialMetadata, action: Action) {
    setBusy(c.ref)
    setNotice(null)
    try {
      if (action === 'enable') await api.enableCredential(c.ref)
      else if (action === 'disable') await api.disableCredential(c.ref)
      else if (action === 'refresh') await api.refreshCredential(c.ref)
      else await api.deleteCredential(c.ref)
      setNotice({ ok: true, text: `${done[action]} ${c.ref}.` })
    } catch (e) {
      setNotice({ ok: false, text: `Could not ${action} ${c.ref}: ${errorMessage(e)}` })
    } finally {
      setBusy(null)
      list.reload()
    }
  }

  return (
    <>
      <PageHeader
        title="Credentials"
        lede="The upstream accounts in the pool, with their health and use. Adding a credential and rotating its material happen with the janus CLI, never in a browser."
        actions={
          <>
            <label className="flex items-center gap-2 text-sm">
              <span className="text-muted">Upstream</span>
              <select
                className="field"
                value={upstream}
                onChange={(e) => setUpstream(e.target.value as UpstreamKind | '')}
              >
                <option value="">All</option>
                {UPSTREAMS.map((u) => (
                  <option key={u} value={u}>
                    {u}
                  </option>
                ))}
              </select>
            </label>
            <button type="button" className="btn" onClick={list.reload} disabled={list.loading}>
              <RefreshCw size={14} aria-hidden="true" /> Reload
            </button>
          </>
        }
      />
      <ActionNotice notice={notice} />
      {list.error !== undefined && (
        <ErrorNotice error={list.error} what="credentials" onRetry={list.reload} />
      )}
      {list.data === undefined ? (
        list.loading && <Loading what="credentials" />
      ) : list.data.length === 0 ? (
        <Empty title={upstream ? `No credentials for ${upstream}` : 'The pool is empty'}>
          Add one from the daemon's host with{' '}
          <code className="mono">janus credential add --upstream &lt;name&gt; --from-file &lt;path&gt;</code>.
        </Empty>
      ) : (
        <div className="overflow-x-auto rounded-md border border-line bg-surface">
          <table className="data-table">
            <thead>
              <tr>
                <th scope="col">Credential</th>
                <th scope="col">Upstream</th>
                <th scope="col">Status</th>
                <th scope="col">Expires</th>
                <th scope="col">Last used</th>
                <th scope="col" className="text-right">
                  Requests
                </th>
                <th scope="col" className="text-right">
                  Errors
                </th>
                <th scope="col">
                  <span className="sr-only">Actions</span>
                </th>
              </tr>
            </thead>
            <tbody>
              {list.data.map((c) => (
                <tr key={c.ref} aria-label={c.ref}>
                  <td>
                    <div className="mono font-medium">{c.ref}</div>
                    {c.label && <div className="text-xs text-ink-soft">{c.label}</div>}
                    <div className="text-xs text-muted">
                      {kindLabel[c.kind] ?? c.kind}, {sourceLabel[c.source] ?? c.source}
                    </div>
                  </td>
                  <td>{c.upstream}</td>
                  <td>
                    <Badge tone={credentialTone(c.status)}>{words(c.status)}</Badge>
                    {c.status === 'rate_limited' && c.rate_limited_until && (
                      <div className="mt-1 text-xs text-muted">
                        until <Time value={c.rate_limited_until} />
                      </div>
                    )}
                    {c.last_error && (
                      <div className="mt-1 max-w-64 text-xs break-words text-bad">{c.last_error}</div>
                    )}
                  </td>
                  <td>
                    <Time value={c.expires_at} empty="does not expire" />
                  </td>
                  <td>
                    <Time value={c.last_used_at} empty="never" />
                  </td>
                  <td className="text-right tabular-nums">{formatCount(c.request_count)}</td>
                  <td className="text-right tabular-nums">{formatCount(c.error_count)}</td>
                  <td>
                    <div className="flex justify-end gap-2">
                      {c.status === 'disabled' ? (
                        <button
                          type="button"
                          className="btn"
                          disabled={busy === c.ref}
                          onClick={() => act(c, 'enable')}
                        >
                          Enable
                        </button>
                      ) : (
                        <button
                          type="button"
                          className="btn"
                          disabled={busy === c.ref}
                          onClick={() => act(c, 'disable')}
                        >
                          Disable
                        </button>
                      )}
                      <button
                        type="button"
                        className="btn"
                        disabled={busy === c.ref}
                        onClick={() => act(c, 'refresh')}
                      >
                        Refresh
                      </button>
                      {c.source === 'pool' ? (
                        <ConfirmAction
                          label="Remove"
                          confirmLabel={`Remove ${c.ref}`}
                          consequence="Deletes the pool slot and its material."
                          busy={busy === c.ref}
                          onConfirm={() => act(c, 'remove')}
                        />
                      ) : (
                        <button
                          type="button"
                          className="btn btn-danger"
                          disabled
                          title="Defined in the daemon config; remove it there."
                        >
                          Remove
                        </button>
                      )}
                    </div>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </>
  )
}
