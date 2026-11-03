import type { ReactNode } from 'react'
import { ApiError, NetworkError, errorMessage } from '@/api/client'

export function Loading({ what }: { what: string }) {
  return (
    <p role="status" className="py-8 text-sm text-muted">
      Loading {what}…
    </p>
  )
}

export function ErrorNotice({
  error,
  what,
  onRetry,
}: {
  error: unknown
  what: string
  onRetry?: () => void
}) {
  const hint =
    error instanceof NetworkError
      ? 'Check that janus-daemon is running and its UI listener is enabled.'
      : error instanceof ApiError && error.status >= 500
        ? 'The daemon log has the details.'
        : null
  return (
    <div role="alert" className="my-4 rounded-md border border-bad/40 bg-bad-soft px-4 py-3 text-sm">
      <p className="font-medium text-bad">Could not load {what}.</p>
      <p className="mt-1 text-ink-soft">{errorMessage(error)}</p>
      {hint && <p className="mt-1 text-muted">{hint}</p>}
      {onRetry && (
        <button type="button" className="btn mt-3" onClick={onRetry}>
          Try again
        </button>
      )}
    </div>
  )
}

export function Empty({ title, children }: { title: string; children?: ReactNode }) {
  return (
    <div className="rounded-md border border-dashed border-line-strong px-6 py-10 text-center">
      <p className="font-serif text-lg">{title}</p>
      {children && <div className="mx-auto mt-2 max-w-prose text-sm text-muted">{children}</div>}
    </div>
  )
}

/** The outcome of the last action, announced to assistive tech. */
export function ActionNotice({ notice }: { notice: { ok: boolean; text: string } | null }) {
  return (
    <div aria-live="polite" className="min-h-6">
      {notice && (
        <p
          role={notice.ok ? 'status' : 'alert'}
          className={`text-sm ${notice.ok ? 'text-ok' : 'text-bad'}`}
        >
          {notice.text}
        </p>
      )}
    </div>
  )
}
