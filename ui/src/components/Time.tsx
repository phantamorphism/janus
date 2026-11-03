import { formatRelative, formatTime, parseTime } from '@/lib/format'

/** An absolute local time with the relative distance beside it. */
export function Time({
  value,
  empty = '—',
  stacked = false,
}: {
  value: string | null | undefined
  empty?: string
  /** Put the relative distance on its own line, for narrow table cells. */
  stacked?: boolean
}) {
  const d = parseTime(value)
  if (!d) return <span className="text-muted">{empty}</span>
  return (
    <time dateTime={d.toISOString()} title={d.toISOString()} className="whitespace-nowrap">
      {formatTime(value)}
      <span className={`text-xs text-muted ${stacked ? 'block' : 'ml-1.5'}`}>{formatRelative(value)}</span>
    </time>
  )
}
