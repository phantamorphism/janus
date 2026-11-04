import { describe, expect, it, vi } from 'vitest'
import { apiError, eventStream, mockApi } from '@/test/http'
import { SseParser, runTail, type SseEvent, type TailState } from './sse'

describe('SseParser', () => {
  it('parses events across chunk boundaries, CRLF/CR line ends and comments; id and retry fields are ignored', () => {
    const p = new SseParser()
    const out: SseEvent[] = []
    out.push(...p.feed(': keepalive\r\nretry: 1500\r'))
    out.push(...p.feed('\nevent: request\r\nid: 7\r\ndata: {"a":'))
    out.push(...p.feed('1}\r\n\r\nevent: ping\rdata\r\r'))
    out.push(...p.feed('data: one\ndata: two\n\n'))
    expect(out).toEqual([
      { event: 'request', data: '{"a":1}' },
      { event: 'ping', data: '' },
      { event: 'message', data: 'one\ntwo' },
    ])
  })

  it('does not dispatch an event block without data', () => {
    const p = new SseParser()
    expect(p.feed('event: request\n\n')).toEqual([])
    expect(p.feed('data: x\n\n')).toEqual([{ event: 'message', data: 'x' }])
  })
})

describe('runTail', () => {
  it('reads with the bearer header, reconnects after the stream ends with the same request and no Last-Event-ID', async () => {
    const ctl = new AbortController()
    const events: SseEvent[] = []
    const states: TailState[] = []
    const sleeps: number[] = []
    let n = 0
    const http = mockApi({
      'GET /v1/requests/stream': () => {
        n++
        const s = eventStream()
        if (n === 1) {
          s.send('event: request\ndata: {"request_id":"r1"}\n\n')
          s.close()
        } else {
          s.send('event: request\ndata: {"request_id":"r2"}\n\n')
          s.close()
          ctl.abort()
        }
        return s.response
      },
    })
    await runTail({
      url: '/v1/requests/stream?upstream=openai',
      headers: { Authorization: 'Bearer tok' },
      signal: ctl.signal,
      onEvent: (e) => events.push(e),
      onState: (s) => states.push(s),
      onUnauthorized: vi.fn(),
      sleep: async (ms) => {
        sleeps.push(ms)
      },
    })
    expect(events.map((e) => JSON.parse(e.data).request_id)).toEqual(['r1', 'r2'])
    const [first, second] = http.calls
    expect(first.headers.get('Authorization')).toBe('Bearer tok')
    expect(first.headers.get('Accept')).toBe('text/event-stream')
    expect(first.headers.get('Last-Event-ID')).toBeNull()
    expect(second.headers.get('Last-Event-ID')).toBeNull()
    expect(second.url).not.toContain('tok')
    expect(sleeps).toEqual([2000])
    expect(states.map((s) => s.phase)).toEqual(['connecting', 'open', 'waiting', 'connecting', 'open'])
  })

  it('backs off exponentially while the daemon is failing', async () => {
    const ctl = new AbortController()
    const sleeps: number[] = []
    mockApi({ 'GET /v1/requests/stream': apiError(503, 'unavailable', 'starting') })
    await runTail({
      url: '/v1/requests/stream',
      headers: {},
      signal: ctl.signal,
      onEvent: vi.fn(),
      onState: vi.fn(),
      onUnauthorized: vi.fn(),
      initialRetryMs: 100,
      maxRetryMs: 500,
      sleep: async (ms) => {
        sleeps.push(ms)
        if (sleeps.length === 5) ctl.abort()
      },
    })
    expect(sleeps).toEqual([100, 200, 400, 500, 500])
  })

  it('stops on 401 and reports it, and stops on a refused filter without retrying', async () => {
    const onUnauthorized = vi.fn()
    const states: TailState[] = []
    const http = mockApi({ 'GET /v1/requests/stream': apiError(401, 'unauthorized', 'no') })
    await runTail({
      url: '/v1/requests/stream',
      headers: {},
      signal: new AbortController().signal,
      onEvent: vi.fn(),
      onState: (s) => states.push(s),
      onUnauthorized,
    })
    expect(onUnauthorized).toHaveBeenCalledOnce()
    expect(http.calls).toHaveLength(1)

    mockApi({ 'GET /v1/requests/stream': apiError(400, 'invalid_filter', 'unknown outcome "nope"') })
    states.length = 0
    await runTail({
      url: '/v1/requests/stream',
      headers: {},
      signal: new AbortController().signal,
      onEvent: vi.fn(),
      onState: (s) => states.push(s),
      onUnauthorized: vi.fn(),
    })
    expect(states.at(-1)).toEqual({ phase: 'stopped', reason: 'unknown outcome "nope"' })
  })
})
