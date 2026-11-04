import { screen, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { describe, expect, it } from 'vitest'
import { apiError, json, mockApi } from '@/test/http'
import { credential, plugin, record, status, credentialList, requestPage } from '@/test/fixtures'
import { renderWithApi } from '@/test/render'
import { Overview } from './Overview'

function routes(over: Record<string, unknown> = {}) {
  return mockApi({
    'GET /v1/status': status(),
    'GET /v1/credentials': credentialList([credential()]),
    'GET /v1/requests': requestPage([]),
    ...over,
  })
}

describe('Overview view', () => {
  it('renders listeners, upstreams with their bindings, and the plugin with certificate expiry', async () => {
    routes({ 'GET /v1/status': status({ plugin: plugin({ client_cert_expires_at: '2020-01-01T00:00:00Z' }) }) })
    renderWithApi(<Overview />)
    const listeners = await screen.findByRole('table', { name: 'Listeners' })
    expect(within(listeners).getByText('127.0.0.1:8080')).toBeInTheDocument()
    expect(within(listeners).getByText('data plane')).toBeInTheDocument()
    expect(within(listeners).getByText('/var/lib/janus/admin.sock')).toBeInTheDocument()

    const upstreams = screen.getByRole('table', { name: 'Upstreams' })
    expect(within(within(upstreams).getByText('openai').closest('tr')!).getByText('hmac')).toBeInTheDocument()
    expect(within(within(upstreams).getByText('anthropic').closest('tr')!).getByText('opaque')).toBeInTheDocument()

    const pluginSection = screen.getByRole('region', { name: 'Plugin' })
    expect(within(pluginSection).getByText('tls')).toBeInTheDocument()
    expect(within(pluginSection).getByText('ok')).toBeInTheDocument()
    expect(within(pluginSection).getByText('Daemon client certificate expires')).toBeInTheDocument()
    expect(within(pluginSection).getByText('expired')).toBeInTheDocument()
    expect(screen.getByText('0.1.0')).toBeInTheDocument()
    expect(await screen.findByText(/5,321 records stored, kept for 30 days/)).toBeInTheDocument()
  })

  it('shows plugin dropped events, routes and listener URLs from the status document', async () => {
    routes({
      'GET /v1/status': status({
        plugin: plugin({ dropped_events: 12 }),
        listeners: {
          admin_socket: '/s.sock',
          admin_tcp: { address: '127.0.0.1:7000', tls: true, url: 'https://127.0.0.1:7000' },
          ui: null,
          data_plane: '127.0.0.1:8080',
        },
      }),
    })
    renderWithApi(<Overview />)
    const pluginSection = await screen.findByRole('region', { name: 'Plugin' })
    expect(within(pluginSection).getByLabelText('Dropped notify events')).toHaveTextContent('12')
    const listeners = screen.getByRole('table', { name: 'Listeners' })
    expect(within(listeners).getByText('https://127.0.0.1:7000')).toBeInTheDocument()
    expect(within(listeners).queryByText('ui')).not.toBeInTheDocument()
    const upstreams = screen.getByRole('table', { name: 'Upstreams' })
    expect(within(upstreams).getByText('https://api.openai.com/')).toBeInTheDocument()
    expect(within(upstreams).getByText(/\/openai\/v1\/embeddings/)).toBeInTheDocument()
  })

  it('shows dropped log records and refused connections, highlighting non-zero', async () => {
    routes({
      'GET /v1/status': status({
        data_plane: { refused_connections: 5 },
        request_log: { enabled: true, retain: '1day', retain_seconds: 86400, rows: 10, dropped_records: 3 },
      }),
    })
    renderWithApi(<Overview />)
    const dropped = await screen.findByLabelText('Dropped log records')
    expect(dropped).toHaveTextContent('3')
    expect(dropped).toHaveClass('text-bad')
    expect(screen.getByLabelText('Refused connections')).toHaveTextContent('5')
  })

  it('shows zero dropped records and refused connections unhighlighted', async () => {
    routes({})
    renderWithApi(<Overview />)
    expect(await screen.findByLabelText('Dropped log records')).not.toHaveClass('text-bad')
    expect(screen.getByLabelText('Refused connections')).toHaveTextContent('0')
  })

  it('summarizes credential health by status and names the ones needing attention', async () => {
    routes({
      'GET /v1/credentials': credentialList([
        credential(),
        credential({ ref: 'b', status: 'active' }),
        credential({ ref: 'c', status: 'invalid', last_error: 'revoked upstream' }),
        credential({ ref: 'd', status: 'disabled' }),
      ]),
    })
    renderWithApi(<Overview />)
    const counts = await screen.findByRole('list', { name: 'Credentials by status' })
    const countFor = (s: string) => within(counts).getByText(s).closest('li')!.textContent
    expect(countFor('active')).toBe('2active')
    expect(countFor('invalid')).toBe('1invalid')
    expect(countFor('disabled')).toBe('1disabled')
    expect(countFor('rate limited')).toBe('0rate limited')
    const attention = screen.getByRole('list', { name: 'Credentials needing attention' })
    expect(attention).toHaveTextContent('c (anthropic) invalid: revoked upstream')
    expect(attention).toHaveTextContent('d (anthropic) disabled')
  })

  it('counts requests and denials over the window, following the cursor across pages', async () => {
    const page1 = [
      record({ request_id: 'a' }),
      record({ request_id: 'b', outcome: 'denied', reason: 'budget' }),
      record({ request_id: 'c', outcome: 'rejected_identity', kind: 'expired' }),
    ]
    const page2 = [record({ request_id: 'd', outcome: 'denied', reason: 'budget' })]
    const http = routes({
      'GET /v1/requests': (c: { query: URLSearchParams }) =>
        c.query.get('before') === 'p2'
          ? json({ requests: page2, next: null, disabled: false })
          : json({ requests: page1, next: 'p2', disabled: false }),
    })
    renderWithApi(<Overview />)
    expect(await screen.findByLabelText('Request count')).toHaveTextContent('4')
    expect(screen.getByLabelText('Denied count')).toHaveTextContent('2')
    expect(screen.getByLabelText('Rejected count')).toHaveTextContent('1')
    expect(screen.getByText(/50\.0% of requests/)).toBeInTheDocument()

    const calls = http.callsTo('GET', '/v1/requests')
    const since = new Date(calls[0].query.get('since')!).getTime()
    expect(Date.now() - since).toBeGreaterThan(59 * 60_000)
    expect(Date.now() - since).toBeLessThan(61 * 60_000)
    expect(calls[0].query.get('limit')).toBe('1000')
    expect(calls[1].query.get('before')).toBe('p2')

    await userEvent.selectOptions(screen.getByLabelText('Window'), '15')
    const latest = http.callsTo('GET', '/v1/requests').at(-1)!
    expect(Date.now() - new Date(latest.query.get('since')!).getTime()).toBeLessThan(16 * 60_000)
  })

  it('says so when the request log is disabled', async () => {
    routes({ 'GET /v1/requests': requestPage([], null, true) })
    renderWithApi(<Overview />)
    expect(await screen.findByText(/The request log is off/)).toBeInTheDocument()
    expect(screen.queryByLabelText('Request count')).not.toBeInTheDocument()
  })

  it('shows a status failure without hiding the rest of the page', async () => {
    routes({ 'GET /v1/status': apiError(500, 'internal', 'status exploded') })
    renderWithApi(<Overview />)
    expect(await screen.findByText(/status exploded/)).toBeInTheDocument()
    expect(screen.getByText('Could not load daemon status.')).toBeInTheDocument()
    expect(await screen.findByRole('list', { name: 'Credentials by status' })).toBeInTheDocument()
  })
})
