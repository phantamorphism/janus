import { act, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { describe, expect, it } from 'vitest'
import { eventStream, json, mockApi, type Call } from '@/test/http'
import { record, requestPage } from '@/test/fixtures'
import { renderWithApi, TOKEN } from '@/test/render'
import type { RequestRecord } from '@/api/types'
import { Requests } from './Requests'

function sse(r: RequestRecord) {
  return `event: request\ndata: ${JSON.stringify(r)}\n\n`
}

function rows() {
  const table = screen.getByRole('table', { name: 'Request log' })
  return within(table)
    .getAllByRole('row')
    .slice(1)
    .map((tr) => tr.getAttribute('data-request-id'))
}

/** A daemon with a log and a live tail the test drives. */
function daemon(initial: RequestRecord[] = [record()], gap: RequestRecord[] = []) {
  const streams: { call: Call; send: (t: string) => void; close: () => void }[] = []
  const http = mockApi({
    // A query with `since` is a gap query (the page load sends none).
    'GET /v1/requests': (c: Call) =>
      json(
        requestPage(
          (c.query.get('since') ? gap : initial).filter(
            (r) => !c.query.get('credential') || r.credential_ref === c.query.get('credential'),
          ),
        ),
      ),
    'GET /v1/requests/stream': (call: Call) => {
      const s = eventStream(call.signal)
      streams.push({ call, send: s.send, close: s.close })
      return s.response
    },
    'GET /v1/requests/req-1': json(record({ usage: { input_tokens: 99 } })),
  })
  return { http, streams }
}

describe('Requests view', () => {
  it('shows the log and sends every filter to the daemon', async () => {
    const http = mockApi({
      'GET /v1/requests': (c: Call) =>
        c.query.get('outcome') === 'denied'
          ? json({ requests: [record({ request_id: 'd1', outcome: 'denied', reason: 'over budget' })], next: null, disabled: false })
          : json({ requests: [record(), record({ request_id: 'req-2', caller_identity: '', claimed_identity: 'sb-9', outcome: 'rejected_identity', kind: 'revoked', status: null })], next: null, disabled: false }),
    })
    renderWithApi(<Requests />)
    await screen.findByRole('table', { name: 'Request log' })
    expect(rows()).toEqual(['req-1', 'req-2'])
    expect(screen.getByText('sb-9?')).toBeInTheDocument()
    expect(screen.getByText('revoked')).toBeInTheDocument()

    await userEvent.type(screen.getByLabelText('Identity'), 'sandbox-7')
    await userEvent.selectOptions(screen.getByLabelText('Outcome'), 'denied')
    await userEvent.selectOptions(screen.getByLabelText('Upstream'), 'openai')
    await userEvent.type(screen.getByLabelText('Credential'), 'team-a')
    await userEvent.type(screen.getByLabelText('Since'), '2026-11-03T09:00:00')
    await userEvent.selectOptions(screen.getByLabelText('Page size'), '250')
    await userEvent.click(screen.getByRole('button', { name: 'Apply filters' }))

    await waitFor(() => expect(rows()).toEqual(['d1']))
    expect(screen.getByText('over budget')).toBeInTheDocument()
    const q = http.calls.at(-1)!.query
    expect(q.get('identity')).toBe('sandbox-7')
    expect(q.get('outcome')).toBe('denied')
    expect(q.get('upstream')).toBe('openai')
    expect(q.get('credential')).toBe('team-a')
    expect(q.get('since')).toBe(new Date('2026-11-03T09:00:00').toISOString())
    expect(q.get('limit')).toBe('250')
    expect(q.has('until')).toBe(false)
  })

  it('pages back through older records with the cursor', async () => {
    const http = mockApi({
      'GET /v1/requests': (c: Call) =>
        c.query.get('before') === 'cur-1'
          ? json({ requests: [record({ request_id: 'old-1' })], next: null, disabled: false })
          : json({ requests: [record({ request_id: 'new-1' })], next: 'cur-1', disabled: false }),
    })
    renderWithApi(<Requests />)
    await userEvent.click(await screen.findByRole('button', { name: 'Load older requests' }))
    await waitFor(() => expect(rows()).toEqual(['new-1', 'old-1']))
    expect(http.calls.at(-1)!.query.get('before')).toBe('cur-1')
    expect(screen.getByText('That is everything matching these filters.')).toBeInTheDocument()
  })

  it('opens a detail drawer with the record fetched fresh, and closes it with Escape', async () => {
    const { http } = daemon()
    renderWithApi(<Requests />)
    await userEvent.click(await screen.findByRole('button', { name: /Show details of POST/ }))
    const dialog = await screen.findByRole('dialog', { name: 'Request' })
    expect(within(dialog).getByText('req-1')).toBeInTheDocument()
    expect(within(dialog).getByText('POST /openai/v1/chat/completions')).toBeInTheDocument()
    expect(within(dialog).queryByText('Claimed identity')).not.toBeInTheDocument()
    expect(within(dialog).getByText('task-42')).toBeInTheDocument()
    expect(within(dialog).getByText('hmac')).toBeInTheDocument()
    expect(within(dialog).getByText('412 ms')).toBeInTheDocument()
    expect(await within(dialog).findByText(/"input_tokens": 99/)).toBeInTheDocument()
    expect(http.callsTo('GET', '/v1/requests/req-1')).toHaveLength(1)

    await userEvent.keyboard('{Escape}')
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument()
  })

  it('tails live over fetch with the token in the header, and stops when switched off', async () => {
    const { streams } = daemon()
    renderWithApi(<Requests />)
    await screen.findByRole('table', { name: 'Request log' })
    await userEvent.selectOptions(screen.getByLabelText('Upstream'), 'openai')
    await userEvent.click(screen.getByRole('button', { name: 'Apply filters' }))
    await userEvent.click(screen.getByRole('button', { name: /Live tail/ }))
    expect(screen.getByRole('button', { name: /Live tail/ })).toHaveAttribute('aria-pressed', 'true')

    await waitFor(() => expect(streams).toHaveLength(1))
    const { call } = streams[0]
    expect(call.headers.get('Authorization')).toBe(`Bearer ${TOKEN}`)
    expect(call.url).not.toContain(TOKEN)
    expect(call.query.get('upstream')).toBe('openai')
    expect(await screen.findByText(/Live: new requests appear/)).toBeInTheDocument()

    act(() => {
      streams[0].send(sse(record({ request_id: 'live-1', received_at: '2026-11-03T11:00:00Z' })))
      streams[0].send('event: ping\ndata: {}\n\n')
      streams[0].send(sse(record({ request_id: 'live-x', upstream: 'anthropic' })))
    })
    await waitFor(() => expect(rows()).toEqual(['live-1', 'req-1']))

    await userEvent.click(screen.getByRole('button', { name: /Live tail/ }))
    expect(call.signal?.aborted).toBe(true)
    expect(screen.queryByText(/Live: new requests appear/)).not.toBeInTheDocument()
  })

  it('applies credential and time filters to live records without reconnecting', async () => {
    const { streams } = daemon()
    renderWithApi(<Requests />)
    await screen.findByRole('table', { name: 'Request log' })
    await userEvent.click(screen.getByRole('button', { name: /Live tail/ }))
    await waitFor(() => expect(streams).toHaveLength(1))
    await userEvent.type(screen.getByLabelText('Credential'), 'team-b')
    await userEvent.click(screen.getByRole('button', { name: 'Apply filters' }))
    expect(await screen.findByText('No requests match')).toBeInTheDocument()
    act(() => {
      streams[0].send(sse(record({ request_id: 'other-cred', credential_ref: 'team-a' })))
      streams[0].send(sse(record({ request_id: 'match', credential_ref: 'team-b' })))
    })
    await waitFor(() => expect(rows()).toEqual(['match']))
    expect(streams).toHaveLength(1)
  })

  it('holds new records while paused and shows them on resume', async () => {
    const { streams } = daemon()
    renderWithApi(<Requests />)
    await screen.findByRole('table', { name: 'Request log' })
    await userEvent.click(screen.getByRole('button', { name: /Live tail/ }))
    await waitFor(() => expect(streams).toHaveLength(1))
    await screen.findByText(/Live: new requests appear/)

    await userEvent.click(screen.getByRole('button', { name: 'Pause' }))
    act(() => {
      streams[0].send(sse(record({ request_id: 'held-1' })))
      streams[0].send(sse(record({ request_id: 'held-2' })))
    })
    expect(await screen.findByRole('button', { name: 'Resume (2 new)' })).toBeInTheDocument()
    expect(rows()).toEqual(['req-1'])
    expect(screen.getByText('Paused, 2 new held back')).toBeInTheDocument()

    await userEvent.click(screen.getByRole('button', { name: 'Resume (2 new)' }))
    expect(rows()).toEqual(['held-2', 'held-1', 'req-1'])
  })

  it('closes the gap with GET /v1/requests?since= after a lagged event, without duplicating rows', async () => {
    const gapRec = record({ request_id: 'gap-1', received_at: '2026-11-03T10:30:00Z' })
    const { http, streams } = daemon([record()], [gapRec, record({ request_id: 'live-1' })])
    renderWithApi(<Requests />)
    await screen.findByRole('table', { name: 'Request log' })
    await userEvent.selectOptions(screen.getByLabelText('Upstream'), 'openai')
    await userEvent.click(screen.getByRole('button', { name: 'Apply filters' }))
    await userEvent.click(screen.getByRole('button', { name: /Live tail/ }))
    await waitFor(() => expect(streams).toHaveLength(1))
    act(() => {
      streams[0].send('event: ping\ndata: {"at":"2026-11-03T10:00:00Z"}\n\n')
      streams[0].send(sse(record({ request_id: 'live-1' })))
    })
    await waitFor(() => expect(rows()).toContain('live-1'))
    expect(http.calls.filter((c) => c.path === '/v1/requests' && c.query.get('since'))).toHaveLength(0)

    act(() => {
      streams[0].send('event: lagged\ndata: {"missed":7}\n\n')
    })
    await waitFor(() => expect(rows()).toContain('gap-1'))
    expect(rows().filter((id) => id === 'live-1')).toHaveLength(1)
    const [gapQuery] = http.calls.filter((c) => c.path === '/v1/requests' && c.query.get('since'))
    expect(gapQuery.query.get('upstream')).toBe('openai')
    expect(gapQuery.query.get('limit')).toBe('1000')
    expect(Date.now() - new Date(gapQuery.query.get('since')!).getTime()).toBeLessThan(10_000)
  })

  it('backfills after a reconnect, once the new stream has pinged', async () => {
    const { http, streams } = daemon([record()], [record({ request_id: 'missed-1' })])
    renderWithApi(<Requests />)
    await screen.findByRole('table', { name: 'Request log' })
    await userEvent.click(screen.getByRole('button', { name: /Live tail/ }))
    await waitFor(() => expect(streams).toHaveLength(1))
    act(() => streams[0].send('event: ping\ndata: {}\n\n'))
    act(() => streams[0].close())
    expect(await screen.findByText(/Connection lost/)).toBeInTheDocument()
    await waitFor(() => expect(streams).toHaveLength(2), { timeout: 5000 })
    const gapQueries = () => http.calls.filter((c) => c.path === '/v1/requests' && c.query.get('since'))
    expect(gapQueries()).toHaveLength(0)
    act(() => streams[1].send('event: ping\ndata: {}\n\n'))
    await waitFor(() => expect(rows()).toContain('missed-1'))
    expect(gapQueries()).toHaveLength(1)
    // A later ping with nothing missing does not query again.
    act(() => streams[1].send('event: ping\ndata: {}\n\n'))
    await new Promise((r) => setTimeout(r, 50))
    expect(gapQueries()).toHaveLength(1)
  })

  it('stops the tail and explains it on a disabled event, without reconnecting', async () => {
    const { streams } = daemon()
    renderWithApi(<Requests />)
    await screen.findByRole('table', { name: 'Request log' })
    await userEvent.click(screen.getByRole('button', { name: /Live tail/ }))
    await waitFor(() => expect(streams).toHaveLength(1))
    act(() => {
      streams[0].send('event: disabled\ndata: {"disabled":true}\n\n')
      streams[0].close()
    })
    expect(await screen.findByText('The request log is off')).toBeInTheDocument()
    expect(screen.getByRole('button', { name: /Live tail/ })).toBeDisabled()
    await new Promise((r) => setTimeout(r, 100))
    expect(streams).toHaveLength(1)
  })

  it('shows an error instead of an empty list when the page has the wrong shape', async () => {
    mockApi({ 'GET /v1/requests': json({ records: [], next: null, disabled: false }) })
    renderWithApi(<Requests />)
    expect(await screen.findByText(/unexpected response shape/)).toBeInTheDocument()
  })

  it('explains a disabled request log and offers no live tail', async () => {
    mockApi({ 'GET /v1/requests': requestPage([], null, true) })
    renderWithApi(<Requests />)
    expect(await screen.findByText('The request log is off')).toBeInTheDocument()
    expect(screen.getByRole('button', { name: /Live tail/ })).toBeDisabled()
  })
})
