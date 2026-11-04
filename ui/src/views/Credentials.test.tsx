import { screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { describe, expect, it } from 'vitest'
import { apiError, json, mockApi } from '@/test/http'
import { credential, credentialList } from '@/test/fixtures'
import { renderWithApi } from '@/test/render'
import type { CredentialMetadata } from '@/api/types'
import { Credentials } from './Credentials'

/** A daemon whose pool changes state the way the endpoints say. */
function pool(initial: CredentialMetadata[]) {
  let creds = [...initial]
  const set = (ref: string, over: Partial<CredentialMetadata>) => {
    creds = creds.map((c) => (c.ref === ref ? { ...c, ...over } : c))
    return json(creds.find((c) => c.ref === ref))
  }
  const routes: Record<string, unknown> = {
    'GET /v1/credentials': (c: { query: URLSearchParams }) => {
      const u = c.query.get('upstream')
      return json(credentialList(u ? creds.filter((x) => x.upstream === u) : creds))
    },
  }
  for (const c of initial) {
    routes[`POST /v1/credentials/${c.ref}/disable`] = () => set(c.ref, { status: 'disabled' })
    routes[`POST /v1/credentials/${c.ref}/enable`] = () => set(c.ref, { status: 'active' })
    routes[`POST /v1/credentials/${c.ref}/refresh`] = () => set(c.ref, { expires_at: '2031-01-01T00:00:00Z' })
    routes[`DELETE /v1/credentials/${c.ref}`] = () => {
      creds = creds.filter((x) => x.ref !== c.ref)
      return new Response(null, { status: 204 })
    }
  }
  return mockApi(routes)
}

const row = (ref: string) => screen.getByRole('row', { name: ref })

describe('Credentials view', () => {
  it('lists credentials with status and counters and offers no way to enter or show material', async () => {
    pool([
      credential({ last_error: 'upstream said 401', status: 'invalid', error_count: 7 }),
      credential({ ref: 'or-main', upstream: 'openrouter', kind: 'api_key', status: 'rate_limited', rate_limited_until: '2030-01-01T00:00:00Z' }),
    ])
    const { container } = renderWithApi(<Credentials />)
    const a = await screen.findByRole('row', { name: 'team-a' })
    expect(within(a).getByText('invalid')).toBeInTheDocument()
    expect(within(a).getByText('1,234')).toBeInTheDocument()
    expect(within(a).getByText('7')).toBeInTheDocument()
    expect(within(a).getByText('upstream said 401')).toBeInTheDocument()
    expect(within(row('or-main')).getByText('rate limited')).toBeInTheDocument()
    expect(within(row('or-main')).getByText(/until/)).toBeInTheDocument()

    expect(container.querySelector('textarea, input[type="password"], input[type="file"]')).toBeNull()
    expect(screen.queryByRole('button', { name: /add|rotate|material/i })).not.toBeInTheDocument()
  })

  it('disables an active credential and enables it again, confirming each against the daemon', async () => {
    const http = pool([credential()])
    renderWithApi(<Credentials />)
    await userEvent.click(await within(await screen.findByRole('row', { name: 'team-a' })).findByRole('button', { name: 'Disable' }))
    expect(await screen.findByText('Disabled team-a.')).toBeInTheDocument()
    expect(http.callsTo('POST', '/v1/credentials/team-a/disable')).toHaveLength(1)
    await waitFor(() => expect(within(row('team-a')).getByText('disabled')).toBeInTheDocument())

    await userEvent.click(within(row('team-a')).getByRole('button', { name: 'Enable' }))
    expect(await screen.findByText('Enabled team-a.')).toBeInTheDocument()
    expect(http.callsTo('POST', '/v1/credentials/team-a/enable')).toHaveLength(1)
    await waitFor(() => expect(within(row('team-a')).getByText('active')).toBeInTheDocument())
  })

  it('refreshes a credential and shows the daemon’s error when a refresh fails', async () => {
    const http = mockApi({
      'GET /v1/credentials': credentialList([credential()]),
      'POST /v1/credentials/team-a/refresh': apiError(502, 'refresh_failed', 'token endpoint returned 400'),
    })
    renderWithApi(<Credentials />)
    await userEvent.click(within(await screen.findByRole('row', { name: 'team-a' })).getByRole('button', { name: 'Refresh' }))
    expect(await screen.findByRole('alert')).toHaveTextContent(
      'Could not refresh team-a: token endpoint returned 400 (refresh_failed, HTTP 502)',
    )
    expect(http.callsTo('POST', '/v1/credentials/team-a/refresh')).toHaveLength(1)
  })

  it('removes a pool credential only after confirmation, and not a config-defined one', async () => {
    const http = pool([credential(), credential({ ref: 'env-key', source: 'env', kind: 'api_key' })])
    renderWithApi(<Credentials />)
    const teamA = await screen.findByRole('row', { name: 'team-a' })
    expect(within(row('env-key')).getByRole('button', { name: 'Remove' })).toBeDisabled()

    await userEvent.click(within(teamA).getByRole('button', { name: 'Remove' }))
    expect(http.callsTo('DELETE', '/v1/credentials/team-a')).toHaveLength(0)
    await userEvent.click(within(teamA).getByRole('button', { name: 'Cancel' }))
    await userEvent.click(within(teamA).getByRole('button', { name: 'Remove' }))
    await userEvent.click(within(teamA).getByRole('button', { name: 'Remove team-a' }))
    expect(await screen.findByText('Removed team-a.')).toBeInTheDocument()
    expect(http.callsTo('DELETE', '/v1/credentials/team-a')).toHaveLength(1)
    await waitFor(() => expect(screen.queryByRole('row', { name: 'team-a' })).not.toBeInTheDocument())
  })

  it('filters by upstream and explains an empty pool', async () => {
    const http = pool([credential()])
    renderWithApi(<Credentials />)
    await screen.findByRole('row', { name: 'team-a' })
    await userEvent.selectOptions(screen.getByLabelText('Upstream'), 'openai')
    expect(await screen.findByText('No credentials for openai')).toBeInTheDocument()
    expect(http.calls.at(-1)?.query.get('upstream')).toBe('openai')
  })

  it('shows a load failure with a retry', async () => {
    let fail = true
    mockApi({
      'GET /v1/credentials': () => (fail ? apiError(500, 'internal', 'pool directory unreadable') : json(credentialList([credential()]))),
    })
    renderWithApi(<Credentials />)
    expect(await screen.findByText(/pool directory unreadable/)).toBeInTheDocument()
    fail = false
    await userEvent.click(screen.getByRole('button', { name: 'Try again' }))
    expect(await screen.findByRole('row', { name: 'team-a' })).toBeInTheDocument()
  })
})
