// `text/event-stream` over `fetch`.
//
// The live tail cannot use EventSource: EventSource cannot set headers, and
// the admin token travels only in `Authorization`
// (`docs/admin-surface#requests-endpoint`). So the stream is read from a
// fetch body and parsed here, following the WHATWG event-stream rules
// (field parsing, CR/LF/CRLF line ends). The daemon's tail is live only: its
// events carry no `id` and it sends no `retry`, so there is nothing to
// resume from. On reconnect the caller closes the gap with
// `GET /v1/requests?since=` (`docs/admin-surface#requests-endpoint`).

export interface SseEvent {
  event: string
  data: string
}

export class SseParser {
  private buffer = ''
  private pendingCR = false
  private data: string[] = []
  private eventType = ''
  private hasData = false

  /** Feed decoded text; returns the events completed by it. */
  feed(chunk: string): SseEvent[] {
    const out: SseEvent[] = []
    let text = chunk
    if (this.pendingCR) {
      this.pendingCR = false
      if (text.startsWith('\n')) text = text.slice(1)
    }
    this.buffer += text
    let start = 0
    for (let i = 0; i < this.buffer.length; i++) {
      const c = this.buffer[i]
      if (c !== '\n' && c !== '\r') continue
      const line = this.buffer.slice(start, i)
      if (c === '\r') {
        if (i + 1 === this.buffer.length) this.pendingCR = true
        else if (this.buffer[i + 1] === '\n') i++
      }
      start = i + 1
      const ev = this.line(line)
      if (ev) out.push(ev)
    }
    this.buffer = this.buffer.slice(start)
    return out
  }

  private line(line: string): SseEvent | null {
    if (line === '') return this.dispatch()
    if (line.startsWith(':')) return null
    const colon = line.indexOf(':')
    const field = colon === -1 ? line : line.slice(0, colon)
    let value = colon === -1 ? '' : line.slice(colon + 1)
    if (value.startsWith(' ')) value = value.slice(1)
    switch (field) {
      case 'event':
        this.eventType = value
        break
      case 'data':
        this.data.push(value)
        this.hasData = true
        break
      default:
        break
    }
    return null
  }

  private dispatch(): SseEvent | null {
    const ev: SseEvent | null = this.hasData
      ? { event: this.eventType || 'message', data: this.data.join('\n') }
      : null
    this.data = []
    this.hasData = false
    this.eventType = ''
    return ev
  }
}

export type TailState =
  | { phase: 'connecting' }
  | { phase: 'open' }
  | { phase: 'waiting'; retryInMs: number; reason: string }
  | { phase: 'stopped'; reason: string }

export interface TailOptions {
  url: string
  headers: Record<string, string>
  signal: AbortSignal
  onEvent: (ev: SseEvent) => void
  onState: (state: TailState) => void
  onUnauthorized: () => void
  /** First reconnect delay; doubles on each consecutive failure. */
  initialRetryMs?: number
  maxRetryMs?: number
  sleep?: (ms: number, signal: AbortSignal) => Promise<void>
}

function abortableSleep(ms: number, signal: AbortSignal): Promise<void> {
  return new Promise((resolve) => {
    const t = setTimeout(done, ms)
    function done() {
      clearTimeout(t)
      signal.removeEventListener('abort', done)
      resolve()
    }
    signal.addEventListener('abort', done)
  })
}

/**
 * Read the event stream until `signal` aborts or the server refuses it.
 * A dropped or ended stream reconnects; a 401 calls `onUnauthorized` and
 * stops; any other 4xx stops with its message (a bad filter does not get
 * better by retrying).
 */
export async function runTail(o: TailOptions): Promise<void> {
  const sleep = o.sleep ?? abortableSleep
  const base = o.initialRetryMs ?? 2000
  const max = o.maxRetryMs ?? 30000
  const parser = new SseParser()
  let failures = 0

  while (!o.signal.aborted) {
    o.onState({ phase: 'connecting' })
    let reason: string
    try {
      const headers: Record<string, string> = {
        ...o.headers,
        Accept: 'text/event-stream',
        'Cache-Control': 'no-cache',
      }
      const res = await fetch(o.url, { headers, signal: o.signal, cache: 'no-store' })
      if (res.status === 401) {
        o.onState({ phase: 'stopped', reason: 'The daemon rejected the admin token.' })
        o.onUnauthorized()
        return
      }
      if (res.status >= 400 && res.status < 500 && res.status !== 408 && res.status !== 429) {
        let message = `HTTP ${res.status}`
        try {
          const body = (await res.json()) as { error?: { message?: string } }
          if (body.error?.message) message = body.error.message
        } catch {
          // Not the JSON error envelope; keep the status.
        }
        o.onState({ phase: 'stopped', reason: message })
        return
      }
      const type = res.headers.get('content-type') ?? ''
      if (!res.ok || !res.body || !type.includes('text/event-stream')) {
        reason = res.ok ? `unexpected response type ${type || '(none)'}` : `HTTP ${res.status}`
      } else {
        o.onState({ phase: 'open' })
        failures = 0
        const reader = res.body.pipeThrough(new TextDecoderStream()).getReader()
        for (;;) {
          const { done, value } = await reader.read()
          if (done) break
          for (const ev of parser.feed(value)) o.onEvent(ev)
        }
        reason = 'The stream ended.'
      }
    } catch (e) {
      if (o.signal.aborted) return
      reason = e instanceof Error ? e.message : String(e)
    }
    if (o.signal.aborted) return
    const delay = Math.min(max, base * 2 ** failures)
    failures++
    o.onState({ phase: 'waiting', retryInMs: delay, reason })
    await sleep(delay, o.signal)
  }
}
