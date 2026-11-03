import { useId, useState, type FormEvent } from 'react'
import { ApiClient, ApiError, errorMessage } from '@/api/client'
import { JanusMark } from '@/components/JanusMark'

/**
 * The admin token prompt. The token is checked against `GET /v1/status`
 * before it is accepted, so a typo is caught here rather than on every view.
 */
export function TokenGate({
  apiBase,
  reason,
  onAccept,
}: {
  apiBase: string
  reason: string | null
  onAccept: (token: string) => void
}) {
  const [token, setToken] = useState('')
  const [checking, setChecking] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const inputId = useId()
  const helpId = useId()

  async function submit(e: FormEvent) {
    e.preventDefault()
    const t = token.trim()
    if (!t) {
      setError('Enter the admin token.')
      return
    }
    setChecking(true)
    setError(null)
    let rejected = false
    const probe = new ApiClient({ baseUrl: apiBase, token: t, onUnauthorized: () => (rejected = true) })
    try {
      await probe.status()
      onAccept(t)
    } catch (err) {
      setChecking(false)
      if (rejected || (err instanceof ApiError && err.status === 401)) {
        setError('The daemon rejected that token. Copy it again from the admin-token file.')
      } else {
        setError(errorMessage(err))
      }
    }
  }

  return (
    <main className="flex min-h-screen items-center justify-center px-4">
      <form onSubmit={submit} className="w-full max-w-md" aria-labelledby={`${inputId}-title`}>
        <div className="mb-6 flex items-center gap-3 text-ink">
          <JanusMark className="h-9 w-9 text-brass" />
          <h1 id={`${inputId}-title`} className="text-3xl">
            Janus
          </h1>
        </div>
        {reason && (
          <p role="alert" className="mb-4 rounded-md bg-warn-soft px-3 py-2 text-sm text-warn">
            {reason}
          </p>
        )}
        <label htmlFor={inputId} className="block text-sm font-medium">
          Admin token
        </label>
        <input
          id={inputId}
          type="password"
          className="field mono mt-1 w-full py-2"
          value={token}
          onChange={(e) => setToken(e.target.value)}
          autoComplete="off"
          spellCheck={false}
          autoFocus
          aria-describedby={helpId}
          aria-invalid={error ? true : undefined}
        />
        <p id={helpId} className="mt-2 text-sm text-muted">
          The daemon generates it into <code className="mono">&lt;data_dir&gt;/admin-token</code> and logs that
          path at startup. It is kept in this tab’s session storage only, and forgotten when the tab closes.
        </p>
        {error && (
          <p role="alert" className="mt-3 text-sm text-bad">
            {error}
          </p>
        )}
        <button type="submit" className="btn btn-primary mt-5 w-full justify-center py-2" disabled={checking}>
          {checking ? 'Checking…' : 'Connect'}
        </button>
      </form>
    </main>
  )
}
