import { screen, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { describe, expect, it } from 'vitest'
import { eventStream, json, mockApi, type Call } from '@/test/http'
import { credential, credentialList, record, requestPage, status } from '@/test/fixtures'
import { renderWithApi } from '@/test/render'
import { Credentials } from './Credentials'
import { Overview } from './Overview'
import { Requests } from './Requests'

// Two instances of one implementation (`docs/configuration#upstream-instances`):
// `/anthropic` for API keys, `/claude` for subscriptions, failing over.
const instances = status({
  upstreams: [
    {
      name: 'anthropic',
      implementation: 'anthropic',
      strategy: 'round_robin',
      failover: false,
      credentials: { kind: 'api_key' },
      eligible: ['ant-key'],
      path_prefix: '/anthropic',
      base_url: 'https://api.anthropic.com/',
      binding: 'hmac',
      routes: [],
    },
    {
      name: 'claude',
      implementation: 'anthropic',
      strategy: 'sticky',
      failover: true,
      credentials: { kind: 'oauth', tags: ['max', 'pro'] },
      eligible: ['max-1', 'max-2'],
      path_prefix: '/claude',
      base_url: 'https://api.anthropic.com/',
      binding: 'hmac',
      routes: [],
    },
    {
      name: 'empty',
      implementation: 'openai',
      strategy: 'ordered',
      failover: false,
      credentials: { refs: ['gone'] },
      eligible: [],
      path_prefix: '/empty',
      base_url: 'https://api.openai.com/',
      binding: 'hmac',
      routes: [],
    },
  ],
})

const failedOver = record({
  request_id: 'fo-1',
  upstream: 'claude',
  implementation: 'anthropic',
  path: '/claude/v1/messages',
  credential_ref: 'max-2',
  attempts: [{ credential_ref: 'max-1', status: 429, verdict: 'rate_limited' }],
})

describe('upstream instances', () => {
  it('lists each instance with its implementation, selection and admitted credentials', async () => {
    mockApi({
      'GET /v1/status': instances,
      'GET /v1/credentials': credentialList([]),
      'GET /v1/requests': requestPage([]),
    })
    renderWithApi(<Overview />)
    const table = await screen.findByRole('table', { name: 'Upstreams' })
    const claude = within(table).getByText('claude').closest('tr')!
    expect(within(claude).getByText('anthropic')).toBeInTheDocument()
    expect(within(claude).getByText('sticky by identity, fails over')).toBeInTheDocument()
    expect(within(claude).getByText('max-1, max-2')).toBeInTheDocument()
    expect(within(claude).getByText('OAuth only; tagged max or pro')).toBeInTheDocument()

    const keys = within(table).getByText('/anthropic').closest('tr')!
    expect(within(keys).getByText('round robin')).toBeInTheDocument()
    expect(within(keys).getByText('API keys only')).toBeInTheDocument()
    // Named after its implementation: no second line repeating it.
    expect(within(keys).queryByText('anthropic', { selector: 'div' })).not.toBeInTheDocument()

    const empty = within(table).getByText('/empty').closest('tr')!
    expect(within(empty).getByText('none admitted')).toBeInTheDocument()
  })

  it('says an unfiltered instance admits every credential of its implementation', async () => {
    mockApi({ 'GET /v1/status': status(), 'GET /v1/credentials': credentialList([]), 'GET /v1/requests': requestPage([]) })
    renderWithApi(<Overview />)
    const table = await screen.findByRole('table', { name: 'Upstreams' })
    expect(within(table).getByText('every openai credential')).toBeInTheDocument()
    expect(within(table).getAllByText('ordered')).toHaveLength(2)
  })

  it('filters credentials by instance name and shows tags', async () => {
    const http = mockApi({
      'GET /v1/status': instances,
      'GET /v1/credentials': (c: Call) =>
        json(
          credentialList(
            c.query.get('upstream') === 'claude'
              ? [credential({ ref: 'max-1', tags: ['max'] })]
              : [credential({ ref: 'max-1', tags: ['max'] }), credential({ ref: 'ant-key', kind: 'api_key' })],
          ),
        ),
    })
    renderWithApi(<Credentials />)
    expect(await screen.findByRole('list', { name: 'Tags of max-1' })).toHaveTextContent('max')
    expect(screen.queryByRole('list', { name: 'Tags of ant-key' })).not.toBeInTheDocument()

    const select = screen.getByLabelText('Upstream')
    await screen.findByRole('option', { name: 'claude' })
    // Instances first, then implementation names no instance has.
    expect(within(select).getAllByRole('option').map((o) => o.textContent)).toEqual([
      'All',
      'anthropic',
      'claude',
      'empty',
      'openai',
      'openrouter',
    ])
    await userEvent.selectOptions(select, 'claude')
    expect(http.callsTo('GET', '/v1/credentials').at(-1)?.query.get('upstream')).toBe('claude')
    expect(await screen.findByRole('row', { name: 'max-1' })).toBeInTheDocument()
    expect(screen.queryByRole('row', { name: 'ant-key' })).not.toBeInTheDocument()
  })

  it('shows a request on its instance, what it failed over from, and filters by instance', async () => {
    mockApi({
      'GET /v1/status': instances,
      'GET /v1/requests': requestPage([failedOver, record()]),
      'GET /v1/requests/fo-1': failedOver,
      'GET /v1/requests/stream': (call: Call) => eventStream(call.signal).response,
    })
    renderWithApi(<Requests />)
    const table = await screen.findByRole('table', { name: 'Request log' })
    const row = within(table).getByText('claude (anthropic)').closest('tr')!
    expect(within(row).getByText('after max-1')).toBeInTheDocument()
    // Named after its implementation: shown once.
    expect(within(table).getByText('openai')).toBeInTheDocument()

    await userEvent.click(within(row).getByText('claude (anthropic)'))
    const drawer = await screen.findByRole('list', { name: 'Failed over from' })
    expect(within(drawer).getByText('max-1')).toBeInTheDocument()
    expect(within(drawer).getByText('429')).toBeInTheDocument()
    expect(within(drawer).getByText('rate limited')).toBeInTheDocument()

    const select = screen.getByLabelText('Upstream')
    await screen.findByRole('option', { name: 'claude' })
    // The log filter matches instance names only.
    expect(within(select).getAllByRole('option').map((o) => o.textContent)).toEqual(['Any', 'anthropic', 'claude', 'empty'])
  })
})
