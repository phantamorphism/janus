import type { ApiClient } from '@/api/client'
import { MAX_LIMIT, OUTCOMES, type OutcomeName } from '@/api/types'

/** Pages read at most when counting a window; past this the counts are a floor. */
export const MAX_PAGES = 10

export interface Traffic {
  disabled: boolean
  total: number
  byOutcome: Record<OutcomeName, number>
  /** More records exist than were read: the counts are lower bounds. */
  truncated: boolean
  minutes: number
}

/** Count the request log over the last `minutes`, by outcome. */
export async function loadTraffic(api: ApiClient, minutes: number, signal: AbortSignal): Promise<Traffic> {
  const since = new Date(Date.now() - minutes * 60_000).toISOString()
  const byOutcome = Object.fromEntries(OUTCOMES.map((o) => [o, 0])) as Record<OutcomeName, number>
  let total = 0
  let before: string | undefined
  for (let page = 0; page < MAX_PAGES; page++) {
    const res = await api.requests({ since, limit: MAX_LIMIT, before }, signal)
    if (res.disabled) return { disabled: true, total: 0, byOutcome, truncated: false, minutes }
    for (const r of res.records) {
      total++
      if (r.outcome in byOutcome) byOutcome[r.outcome]++
    }
    if (!res.next) return { disabled: false, total, byOutcome, truncated: false, minutes }
    before = res.next
  }
  return { disabled: false, total, byOutcome, truncated: true, minutes }
}

