import { screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { describe, expect, it } from 'vitest'
import { json, mockApi } from '@/test/http'
import { identity } from '@/test/fixtures'
import { renderWithApi } from '@/test/render'
import type { IdentityRecord } from '@/api/types'
import { Identities } from './Identities'

function registry(entries: IdentityRecord[]) {
  let rows = [...entries]
  const routes: Record<string, unknown> = {
    'GET /v1/identities': (c: { query: URLSearchParams }) => {
      const now = Date.now()
      return json(
        rows.filter(
          (r) =>
            (!c.query.get('identity') || r.identity === c.query.get('identity')) &&
            (c.query.get('include_revoked') === 'true' || !r.revoked_at) &&
            (c.query.get('include_expired') === 'true' || !r.expires_at || new Date(r.expires_at).getTime() > now),
        ),
      )
    },
  }
  for (const e of entries) {
    routes[`POST /v1/identities/${e.id}/revoke`] = () => {
      rows = rows.map((r) => (r.id === e.id ? { ...r, revoked_at: new Date().toISOString() } : r))
      return json(rows.find((r) => r.id === e.id))
    }
  }
  return mockApi(routes)
}

describe('Identities view', () => {
  it('lists registry entries with claims and state, with no create control and no token', async () => {
    registry([identity()])
    const { container } = renderWithApi(<Identities />)
    const r = await screen.findByRole('row', { name: /sandbox-7/ })
    expect(within(r).getByText('team=blue')).toBeInTheDocument()
    expect(within(r).getByText('active')).toBeInTheDocument()
    expect(within(r).getByText('CI sandbox')).toBeInTheDocument()
    expect(screen.queryByRole('button', { name: /create|new|register|issue/i })).not.toBeInTheDocument()
    expect(container.textContent).not.toMatch(/token:/i)
  })

  it('applies the identity, include-revoked and include-expired filters', async () => {
    const http = registry([
      identity(),
      identity({ id: 'idn_02', identity: 'sandbox-8', revoked_at: '2026-11-02T00:00:00Z' }),
      identity({ id: 'idn_03', identity: 'sandbox-9', expires_at: '2026-01-01T00:00:00Z' }),
    ])
    renderWithApi(<Identities />)
    await screen.findByRole('row', { name: /sandbox-7/ })
    expect(screen.queryByRole('row', { name: /sandbox-8/ })).not.toBeInTheDocument()

    await userEvent.click(screen.getByLabelText('Include revoked'))
    const revoked = await screen.findByRole('row', { name: /sandbox-8/ })
    expect(within(revoked).getByText('revoked')).toBeInTheDocument()
    expect(within(revoked).queryByRole('button', { name: 'Revoke' })).not.toBeInTheDocument()

    await userEvent.click(screen.getByLabelText('Include expired'))
    const expired = await screen.findByRole('row', { name: /sandbox-9/ })
    expect(within(expired).getByText('expired')).toBeInTheDocument()
    expect(http.calls.at(-1)?.query.get('include_expired')).toBe('true')
    expect(http.calls.at(-1)?.query.get('include_revoked')).toBe('true')

    await userEvent.type(screen.getByLabelText('Identity (exact)'), 'sandbox-9')
    await userEvent.click(screen.getByRole('button', { name: 'Filter' }))
    await waitFor(() => expect(screen.queryByRole('row', { name: /sandbox-7/ })).not.toBeInTheDocument())
    expect(http.calls.at(-1)?.query.get('identity')).toBe('sandbox-9')
  })

  it('revokes an entry after confirmation', async () => {
    const http = registry([identity()])
    renderWithApi(<Identities />)
    const r = await screen.findByRole('row', { name: /sandbox-7/ })
    await userEvent.click(within(r).getByRole('button', { name: 'Revoke' }))
    expect(http.callsTo('POST', '/v1/identities/idn_01/revoke')).toHaveLength(0)
    await userEvent.click(within(r).getByRole('button', { name: 'Revoke sandbox-7' }))
    expect(await screen.findByText(/Revoked sandbox-7 \(idn_01\)/)).toBeInTheDocument()
    expect(http.callsTo('POST', '/v1/identities/idn_01/revoke')).toHaveLength(1)
    expect(await screen.findByText('No identities registered')).toBeInTheDocument()
  })

  it('shows a filter-specific empty state', async () => {
    registry([])
    renderWithApi(<Identities />)
    expect(await screen.findByText('No identities registered')).toBeInTheDocument()
    expect(screen.getByText(/Revoked and expired entries are hidden/)).toBeInTheDocument()
  })
})
