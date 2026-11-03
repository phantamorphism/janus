import { useState, type FormEvent } from 'react'
import { errorMessage } from '@/api/client'
import { useApi } from '@/api/context'
import type { IdentityFilter, IdentityRecord } from '@/api/types'
import { Badge } from '@/components/Badge'
import { ConfirmAction } from '@/components/ConfirmAction'
import { PageHeader } from '@/components/PageHeader'
import { ActionNotice, Empty, ErrorNotice, Loading } from '@/components/States'
import { Time } from '@/components/Time'
import { parseTime } from '@/lib/format'
import type { Tone } from '@/lib/tone'
import { useResource } from '@/lib/useResource'

function stateOf(r: IdentityRecord, now: number): { label: string; tone: Tone } {
  if (r.revoked_at) return { label: 'revoked', tone: 'bad' }
  const exp = parseTime(r.expires_at)
  if (exp && exp.getTime() <= now) return { label: 'expired', tone: 'idle' }
  return { label: 'active', tone: 'ok' }
}

export function Identities() {
  const api = useApi()
  const [identityInput, setIdentityInput] = useState('')
  const [filter, setFilter] = useState<IdentityFilter>({
    identity: '',
    include_revoked: false,
    include_expired: false,
  })
  const list = useResource(
    async (signal) => ({ records: await api.identities(filter, signal), at: Date.now() }),
    `identities:${JSON.stringify(filter)}`,
  )
  const [busy, setBusy] = useState<string | null>(null)
  const [notice, setNotice] = useState<{ ok: boolean; text: string } | null>(null)
  function apply(e: FormEvent) {
    e.preventDefault()
    setFilter((f) => ({ ...f, identity: identityInput.trim() }))
  }

  async function revoke(r: IdentityRecord) {
    setBusy(r.id)
    setNotice(null)
    try {
      await api.revokeIdentity(r.id)
      setNotice({ ok: true, text: `Revoked ${r.identity} (${r.id}). Its token is refused from the next request.` })
    } catch (e) {
      setNotice({ ok: false, text: `Could not revoke ${r.identity}: ${errorMessage(e)}` })
    } finally {
      setBusy(null)
      list.reload()
    }
  }

  return (
    <>
      <PageHeader
        title="Identities"
        lede="The opaque-identity registry: who each issued token speaks for. Your provisioner creates entries when it creates a sandbox; here you can see them and revoke them."
      />
      <form onSubmit={apply} className="mb-4 flex flex-wrap items-end gap-x-5 gap-y-3" aria-label="Filter identities">
        <label className="flex flex-col gap-1 text-sm">
          <span className="text-muted">Identity (exact)</span>
          <input
            className="field mono w-64"
            value={identityInput}
            onChange={(e) => setIdentityInput(e.target.value)}
            spellCheck={false}
            autoComplete="off"
          />
        </label>
        <button type="submit" className="btn">
          Filter
        </button>
        <label className="flex items-center gap-2 text-sm">
          <input
            type="checkbox"
            checked={filter.include_revoked}
            onChange={(e) => setFilter((f) => ({ ...f, include_revoked: e.target.checked }))}
          />
          Include revoked
        </label>
        <label className="flex items-center gap-2 text-sm">
          <input
            type="checkbox"
            checked={filter.include_expired}
            onChange={(e) => setFilter((f) => ({ ...f, include_expired: e.target.checked }))}
          />
          Include expired
        </label>
      </form>
      <ActionNotice notice={notice} />
      {list.error !== undefined && (
        <ErrorNotice error={list.error} what="identities" onRetry={list.reload} />
      )}
      {list.data === undefined ? (
        list.loading && <Loading what="identities" />
      ) : list.data.records.length === 0 ? (
        <Empty title={filter.identity ? `No entries for ${filter.identity}` : 'No identities registered'}>
          {filter.include_revoked && filter.include_expired
            ? 'Entries appear here when your provisioner registers them with POST /v1/identities.'
            : 'Revoked and expired entries are hidden; include them with the checkboxes above.'}
        </Empty>
      ) : (
        <div className="overflow-x-auto rounded-md border border-line bg-surface">
          <table className="data-table">
            <thead>
              <tr>
                <th scope="col">Identity</th>
                <th scope="col">Claims</th>
                <th scope="col">State</th>
                <th scope="col">Created</th>
                <th scope="col">Expires</th>
                <th scope="col">Last seen</th>
                <th scope="col">
                  <span className="sr-only">Actions</span>
                </th>
              </tr>
            </thead>
            <tbody>
              {list.data.records.map((r) => {
                const state = stateOf(r, list.data?.at ?? 0)
                const claims = Object.entries(r.claims ?? {})
                return (
                  <tr key={r.id} aria-label={`${r.identity} ${r.id}`}>
                    <td>
                      <div className="mono font-medium">{r.identity}</div>
                      {r.label && <div className="text-xs text-ink-soft">{r.label}</div>}
                      <div className="mono text-xs text-muted">id {r.id}</div>
                    </td>
                    <td>
                      {claims.length === 0 ? (
                        <span className="text-muted">none</span>
                      ) : (
                        <ul className="flex flex-wrap gap-1">
                          {claims.map(([k, v]) => (
                            <li key={k} className="mono rounded bg-sunken px-1.5 py-0.5 text-xs">
                              {k}={v}
                            </li>
                          ))}
                        </ul>
                      )}
                    </td>
                    <td>
                      <Badge tone={state.tone}>{state.label}</Badge>
                      {r.revoked_at && (
                        <div className="mt-1 text-xs text-muted">
                          <Time value={r.revoked_at} />
                        </div>
                      )}
                    </td>
                    <td>
                      <Time value={r.created_at} />
                    </td>
                    <td>
                      <Time value={r.expires_at} empty="never" />
                    </td>
                    <td>
                      <Time value={r.last_seen_at} empty="not yet" />
                    </td>
                    <td className="text-right">
                      {!r.revoked_at && (
                        <ConfirmAction
                          label="Revoke"
                          confirmLabel={`Revoke ${r.identity}`}
                          consequence="The next request with this token is rejected."
                          busy={busy === r.id}
                          onConfirm={() => revoke(r)}
                        />
                      )}
                    </td>
                  </tr>
                )
              })}
            </tbody>
          </table>
        </div>
      )}
    </>
  )
}
