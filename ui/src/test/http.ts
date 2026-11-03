// A fake admin API at the HTTP boundary: `fetch` is replaced, everything
// above it (ApiClient, SSE reader, views) is the real code.

import { vi } from 'vitest'

export interface Call {
  method: string
  path: string
  query: URLSearchParams
  headers: Headers
  body: unknown
  signal: AbortSignal | null | undefined
  url: string
}

type Handler = (call: Call) => Response | Promise<Response>
type Route = Handler | unknown

export function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { 'Content-Type': 'application/json' },
  })
}

export function apiError(status: number, code: string, message: string): Response {
  return json({ error: { code, message } }, status)
}

/** A controllable `text/event-stream` body. */
export function eventStream(signal?: AbortSignal | null) {
  let controller!: ReadableStreamDefaultController<Uint8Array>
  const enc = new TextEncoder()
  const body = new ReadableStream<Uint8Array>({
    start(c) {
      controller = c
    },
  })
  signal?.addEventListener('abort', () => {
    try {
      controller.error(new DOMException('The operation was aborted.', 'AbortError'))
    } catch {
      // Already closed.
    }
  })
  return {
    response: new Response(body, { status: 200, headers: { 'Content-Type': 'text/event-stream' } }),
    send(text: string) {
      controller.enqueue(enc.encode(text))
    },
    close() {
      controller.close()
    },
  }
}

/**
 * Install a fake `fetch`. Routes are keyed `"METHOD /path"` (path without
 * the query); a Response is served as is (cloned per call), a function is
 * called with the request, any other value is served as JSON. Unrouted requests answer 404 in the API's error shape.
 */
export function mockApi(routes: Record<string, Route>) {
  const calls: Call[] = []
  const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = new URL(String(input), 'http://janus.test')
    const call: Call = {
      method: (init?.method ?? 'GET').toUpperCase(),
      path: url.pathname,
      query: url.searchParams,
      headers: new Headers(init?.headers),
      body: typeof init?.body === 'string' ? JSON.parse(init.body) : undefined,
      signal: init?.signal,
      url: String(input),
    }
    calls.push(call)
    if (init?.signal?.aborted) throw new DOMException('The operation was aborted.', 'AbortError')
    const route = routes[`${call.method} ${call.path}`]
    if (route === undefined) return apiError(404, 'not_found', `no route ${call.method} ${call.path}`)
    if (route instanceof Response) return route.clone()
    if (typeof route === 'function') return (route as Handler)(call)
    return json(route)
  })
  vi.stubGlobal('fetch', fetchMock)
  return {
    calls,
    fetchMock,
    callsTo(method: string, path: string) {
      return calls.filter((c) => c.method === method && c.path === path)
    },
  }
}
