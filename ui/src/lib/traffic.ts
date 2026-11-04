import type { ApiClient } from '@/api/client'
import { MAX_LIMIT, OUTCOMES, type OutcomeName } from '@/api/types'

/** Pages read at most when counting a window; past this the counts are a floor. */
export const MAX_PAGES = 10

export interface Traffic {
  disabled: boolean
  total: number
  byOutcome: Record<OutcomeName, number>
  /** Forwarded requests whose upstream reply was a 4xx or a 5xx (outcome stays `forwarded`). */
  upstreamReplies: { clientError: number; serverError: number }
  /** More records exist than were read: the counts are lower bounds. */
  truncated: boolean
  minutes: number
}

/** Count the request log over the last `minutes`, by outcome. */
export async function loadTraffic(api: ApiClient, minutes: number, signal: AbortSignal): Promise<Traffic> {
  const since = new Date(Date.now() - minutes * 60_000).toISOString()
  const byOutcome = Object.fromEntries(OUTCOMES.map((o) => [o, 0])) as Record<OutcomeName, number>
  const upstreamReplies = { clientError: 0, serverError: 0 }
  let total = 0
  let before: string | undefined
  for (let page = 0; page < MAX_PAGES; page++) {
    const res = await api.requests({ since, limit: MAX_LIMIT, before }, signal)
    if (res.disabled) return { disabled: true, total: 0, byOutcome, upstreamReplies, truncated: false, minutes }
    for (const r of res.requests) {
      total++
      if (r.outcome in byOutcome) byOutcome[r.outcome]++
      if (r.outcome === 'forwarded' && r.status !== null) {
        if (r.status >= 500) upstreamReplies.serverError++
        else if (r.status >= 400) upstreamReplies.clientError++
      }
    }
    if (!res.next) return { disabled: false, total, byOutcome, upstreamReplies, truncated: false, minutes }
    before = res.next
  }
  return { disabled: false, total, byOutcome, upstreamReplies, truncated: true, minutes }
}

