import { screen, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { describe, expect, it } from 'vitest'
import { apiError, mockApi } from '@/test/http'
import { plugin } from '@/test/fixtures'
import { renderWithApi } from '@/test/render'
import { Plugin } from './Plugin'

describe('Plugin view', () => {
  it('shows health, mode, endpoint, last error and both certificate expiries', async () => {
    mockApi({
      'GET /v1/plugin': plugin({ health: 'rejected_client_cert', last_error: 'bad certificate alert' }),
    })
    renderWithApi(<Plugin />)
    expect(await screen.findByText('rejected client cert')).toBeInTheDocument()
    expect(screen.getByText('https://policy.internal:7443')).toBeInTheDocument()
    expect(screen.getByText('bad certificate alert')).toBeInTheDocument()
    expect(screen.getByText('Daemon client certificate expires')).toBeInTheDocument()
    expect(screen.getByText('Plugin server certificate expires')).toBeInTheDocument()
  })

  it('runs the channel check and shows every step with its verdict and detail', async () => {
    const http = mockApi({
      'GET /v1/plugin': plugin(),
      'POST /v1/plugin/check': {
        mode: 'tls',
        endpoint: 'https://policy.internal:7443',
        passed: false,
        steps: [
          { step: 'handshake', passed: true, detail: 'handshake accepted' },
          { step: 'throwaway_cert_rejected', passed: true, detail: 'rejected with bad_certificate' },
          { step: 'no_cert_rejected', passed: false, detail: 'accepted without requiring a certificate' },
        ],
      },
    })
    renderWithApi(<Plugin />)
    await userEvent.click(await screen.findByRole('button', { name: 'Run channel check' }))
    const steps = await screen.findByRole('list', { name: 'Check steps' })
    const items = within(steps).getAllByRole('listitem')
    expect(items).toHaveLength(3)
    expect(within(items[0]).getByText('passed')).toBeInTheDocument()
    expect(within(items[1]).getByText('rejected with bad_certificate')).toBeInTheDocument()
    expect(items[2]).toHaveAccessibleName('No certificate refused')
    expect(within(items[2]).getByText('failed')).toBeInTheDocument()
    expect(within(items[2]).getByText('accepted without requiring a certificate')).toBeInTheDocument()
    expect(screen.getByText(/At least one check failed/)).toBeInTheDocument()
    expect(http.callsTo('POST', '/v1/plugin/check')).toHaveLength(1)
    expect(http.callsTo('GET', '/v1/plugin').length).toBeGreaterThanOrEqual(2)
  })

  it('shows the socket path walk with octal modes and failure class', async () => {
    mockApi({
      'GET /v1/plugin': plugin({ mode: 'unix', endpoint: 'unix:///run/policy/p.sock' }),
      'POST /v1/plugin/check': {
        mode: 'unix',
        endpoint: 'unix:///run/policy/p.sock',
        passed: false,
        steps: [
          {
            step: 'path_permissions',
            passed: true,
            detail: 'every directory is safe',
            walk: { socket: '/run/policy/p.sock', dirs: [{ path: '/run/policy', uid: 998, mode: 0o750 }] },
          },
          { step: 'connect', passed: false, detail: 'connection refused', class: 'plugin_unreachable' },
        ],
      },
    })
    renderWithApi(<Plugin />)
    await userEvent.click(await screen.findByRole('button', { name: 'Run channel check' }))
    expect(await screen.findByText('0750')).toBeInTheDocument()
    expect(screen.getByText('/run/policy')).toBeInTheDocument()
    expect(screen.getByText('Failure class: plugin_unreachable')).toBeInTheDocument()
  })

  it('cannot run a check without a plugin, and explains a linked plugin', async () => {
    mockApi({ 'GET /v1/plugin': plugin({ mode: 'none', endpoint: null, health: 'unknown' }) })
    renderWithApi(<Plugin />)
    expect(await screen.findByRole('button', { name: 'Run channel check' })).toBeDisabled()
    expect(screen.getByText(/No plugin is configured/)).toBeInTheDocument()
  })

  it('shows a failed check request as an error', async () => {
    mockApi({
      'GET /v1/plugin': plugin(),
      'POST /v1/plugin/check': apiError(500, 'check_failed', 'could not build probe certificate'),
    })
    renderWithApi(<Plugin />)
    await userEvent.click(await screen.findByRole('button', { name: 'Run channel check' }))
    expect(await screen.findByText(/could not build probe certificate/)).toBeInTheDocument()
  })
})
