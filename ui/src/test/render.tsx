import { render } from '@testing-library/react'
import type { ReactElement } from 'react'
import { vi } from 'vitest'
import { ApiClient } from '@/api/client'
import { ApiContext } from '@/api/context'

export const TOKEN = 'test-admin-token-5f2c'

/** Render a view with a real ApiClient whose fetch is the test's fake. */
export function renderWithApi(ui: ReactElement) {
  const onUnauthorized = vi.fn()
  const api = new ApiClient({ baseUrl: '/v1', token: TOKEN, onUnauthorized })
  return { ...render(<ApiContext.Provider value={api}>{ui}</ApiContext.Provider>), api, onUnauthorized }
}
