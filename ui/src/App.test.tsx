import { render, screen, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { beforeEach, describe, expect, it } from 'vitest'
import App from './App'
import { apiError, mockApi } from '@/test/http'
import { credential, plugin, status } from '@/test/fixtures'

const TOKEN = 'admin-tok-9a1'

function daemon(acceptedToken = TOKEN) {
  const ok = <T,>(body: T) => (c: { headers: Headers }) =>
    c.headers.get('Authorization') === `Bearer ${acceptedToken}`
      ? new Response(JSON.stringify(body), { headers: { 'Content-Type': 'application/json' } })
      : apiError(401, 'unauthorized', 'missing or wrong admin token')
  return mockApi({
    'GET /v1/status': ok(status()),
    'GET /v1/credentials': ok([credential()]),
    'GET /v1/requests': ok({ records: [], next: null, disabled: false }),
    'GET /v1/plugin': ok(plugin()),
    'GET /v1/identities': ok([]),
  })
}

describe('token flow', () => {
  beforeEach(() => {
    window.history.replaceState(null, '', '/')
  })

  it('prompts on first visit, keeps the accepted token in sessionStorage only, and sends it on every call', async () => {
    const http = daemon()
    render(<App />)
    expect(screen.getByLabelText('Admin token')).toBeInTheDocument()
    expect(http.calls).toHaveLength(0)

    await userEvent.type(screen.getByLabelText('Admin token'), TOKEN)
    await userEvent.click(screen.getByRole('button', { name: 'Connect' }))

    expect(await screen.findByRole('heading', { name: 'Overview', level: 1 })).toBeInTheDocument()
    expect(window.sessionStorage.getItem('janus.admin-token')).toBe(TOKEN)
    expect(Object.values({ ...window.localStorage })).not.toContain(TOKEN)
    expect(document.cookie).not.toContain(TOKEN)
    expect(window.location.href).not.toContain(TOKEN)
    await screen.findByText('127.0.0.1:8080')
    expect(http.calls.length).toBeGreaterThan(1)
    for (const c of http.calls) {
      expect(c.headers.get('Authorization')).toBe(`Bearer ${TOKEN}`)
      expect(c.url).not.toContain(TOKEN)
    }
  })

  it('does not prompt again when the tab already holds a token', async () => {
    daemon()
    window.sessionStorage.setItem('janus.admin-token', TOKEN)
    render(<App />)
    expect(screen.queryByLabelText('Admin token')).not.toBeInTheDocument()
    expect(await screen.findByRole('heading', { name: 'Overview', level: 1 })).toBeInTheDocument()
  })

  it('refuses a wrong token at the prompt without storing it', async () => {
    daemon()
    render(<App />)
    await userEvent.type(screen.getByLabelText('Admin token'), 'wrong')
    await userEvent.click(screen.getByRole('button', { name: 'Connect' }))
    expect(await screen.findByText(/rejected that token/)).toBeInTheDocument()
    expect(window.sessionStorage.getItem('janus.admin-token')).toBeNull()
  })

  it('returns to the prompt and forgets the token when the daemon answers 401 mid-session', async () => {
    daemon('rotated-token')
    window.sessionStorage.setItem('janus.admin-token', TOKEN)
    render(<App />)
    expect(await screen.findByText(/no longer accepts the saved token/)).toBeInTheDocument()
    expect(screen.getByLabelText('Admin token')).toBeInTheDocument()
    expect(window.sessionStorage.getItem('janus.admin-token')).toBeNull()
  })

  it('navigates between the five views with real paths', async () => {
    daemon()
    window.sessionStorage.setItem('janus.admin-token', TOKEN)
    render(<App />)
    const nav = screen.getByRole('navigation', { name: 'Views' })
    const links = within(nav).getAllByRole('link').map((a) => a.textContent)
    expect(links).toEqual(['Overview', 'Credentials', 'Identities', 'Requests', 'Plugin'])

    await userEvent.click(within(nav).getByRole('link', { name: 'Plugin' }))
    expect(window.location.pathname).toBe('/plugin')
    expect(await screen.findByRole('heading', { name: 'Plugin', level: 1 })).toBeInTheDocument()
    expect(within(nav).getByRole('link', { name: 'Plugin' })).toHaveAttribute('aria-current', 'page')

    await userEvent.click(screen.getByRole('button', { name: /Forget token/ }))
    expect(screen.getByLabelText('Admin token')).toBeInTheDocument()
    expect(window.sessionStorage.getItem('janus.admin-token')).toBeNull()
  })
})
