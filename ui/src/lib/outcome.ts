import type { RequestFilter, RequestRecord } from '@/api/types'
import { words } from './format'

/** The record's qualifier (`kind`, `reason`, `class`, `stage`), if its outcome has one. */
export function outcomeQualifier(r: RequestRecord): string | null {
  switch (r.outcome) {
    case 'rejected_identity':
      return words(r.kind)
    case 'denied':
      return r.reason
    case 'plugin_failed':
      return words(r.class.replace(/^plugin_/, ''))
    case 'abandoned':
      return `at ${r.stage}`
    default:
      return null
  }
}

/**
 * The same predicate the daemon applies (`RequestFilter::matches`): used on
 * live-tail records for the filters the stream endpoint does not take
 * (`credential`, `since`, `until`).
 */
export function matchesFilter(f: RequestFilter, r: RequestRecord): boolean {
  if (f.identity && f.identity !== r.caller_identity) return false
  if (f.upstream && f.upstream !== r.upstream) return false
  if (f.outcome && f.outcome !== r.outcome) return false
  if (f.credential && f.credential !== r.credential_ref) return false
  const at = new Date(r.received_at).getTime()
  if (f.since && at < new Date(f.since).getTime()) return false
  if (f.until && at >= new Date(f.until).getTime()) return false
  return true
}
