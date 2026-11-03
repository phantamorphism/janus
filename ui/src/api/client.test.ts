import { describe, expect, it, vi } from 'vitest'
import { apiError, json, mockApi } from '@/test/http'
import { credential, record } from '@/test/fixtures'
import { ApiClient, ApiError, NetworkError } from './client'

function client(onUnauthorized = vi.fn()) {
  return new ApiClient({ baseUrl: '/v1', token: 'tok-1', onUnauthorized })
}

describe('ApiClient', () => {
  it('sends the token as a bearer header and the filter in the query, never the token', async () => {
    const http = mockApi({ 'GET /v1/credentials': [credential()] })
    const list = await client().credentials('openai')
    expect(list).toHaveLength(1)
    const [call] = http.calls
    expect(call.headers.get('Authorization')).toBe('Bearer tok-1')
    expect(call.query.get('upstream')).toBe('openai')
    expect(call.url).not.toContain('tok-1')
  })

  it('decodes the JSON error envelope into code, message and status', async () => {
    mockApi({ 'POST /v1/credentials/team-a/refresh': apiError(409, 'refresh_failed', 'token endpoint said no') })
    const err = await client().refreshCredential('team-a').catch((e: unknown) => e)
    expect(err).toBeInstanceOf(ApiError)
    expect(err).toMatchObject({ status: 409, code: 'refresh_failed', message: 'token endpoint said no' })
  })

  it('reports a 401 to onUnauthorized and rejects', async () => {
    mockApi({ 'GET /v1/plugin': apiError(401, 'unauthorized', 'bad token') })
    const onUnauthorized = vi.fn()
    await expect(client(onUnauthorized).plugin()).rejects.toBeInstanceOf(ApiError)
    expect(onUnauthorized).toHaveBeenCalledOnce()
  })

  it('turns a transport failure into a NetworkError naming the URL', async () => {
    vi.stubGlobal('fetch', vi.fn(async () => Promise.reject(new TypeError('Failed to fetch'))))
    const err = await client().status().catch((e: unknown) => e)
    expect(err).toBeInstanceOf(NetworkError)
    expect((err as Error).message).toContain('/v1/status')
  })

  it('reads a request page with its cursor and the disabled marker, and encodes path segments', async () => {
    const http = mockApi({
      'GET /v1/requests': json({ records: [record()], next: 'Y3Vyc29y', disabled: false }),
      'GET /v1/requests/stream': json({}),
    })
    const page = await client().requests({ outcome: 'denied', limit: 50, before: 'abc' })
    expect(page).toEqual({ records: [record()], next: 'Y3Vyc29y', disabled: false })
    expect(Object.fromEntries(http.calls[0].query)).toEqual({ outcome: 'denied', limit: '50', before: 'abc' })

    mockApi({ 'GET /v1/requests': json({ records: [], next: null, disabled: true }) })
    expect(await client().requests({})).toEqual({ records: [], next: null, disabled: true })

    const del = mockApi({})
    await client().deleteCredential('a/b').catch(() => undefined)
    expect(del.calls[0].url).toBe('/v1/credentials/a%2Fb')
  })
})
