// Display formatting shared by the views. Absolute times are local and
// unambiguous; relative times ("3 min ago") sit beside them, never alone.

export function parseTime(ts: string | null | undefined): Date | null {
  if (!ts) return null
  const d = new Date(ts)
  return Number.isNaN(d.getTime()) ? null : d
}

export function formatTime(ts: string | null | undefined): string {
  const d = parseTime(ts)
  if (!d) return '—'
  const pad = (n: number) => String(n).padStart(2, '0')
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(
    d.getMinutes(),
  )}:${pad(d.getSeconds())}`
}

export function formatRelative(ts: string | null | undefined, now: number = Date.now()): string {
  const d = parseTime(ts)
  if (!d) return ''
  const diff = d.getTime() - now
  const abs = Math.abs(diff)
  const units: [number, string][] = [
    [86_400_000, 'd'],
    [3_600_000, 'h'],
    [60_000, 'min'],
    [1000, 's'],
  ]
  for (const [ms, unit] of units) {
    if (abs >= ms) {
      const n = Math.floor(abs / ms)
      return diff < 0 ? `${n} ${unit} ago` : `in ${n} ${unit}`
    }
  }
  return 'just now'
}

export function formatDuration(ms: number | null | undefined): string {
  if (ms === null || ms === undefined) return '—'
  if (ms < 1000) return `${ms} ms`
  if (ms < 60_000) return `${(ms / 1000).toFixed(ms < 10_000 ? 2 : 1)} s`
  return `${Math.floor(ms / 60_000)} min ${Math.round((ms % 60_000) / 1000)} s`
}

export function formatCount(n: number): string {
  return new Intl.NumberFormat('en').format(n)
}

/** `snake_case` vocabulary for reading: `rate_limited` becomes `rate limited`. */
export function words(s: string): string {
  return s.replace(/_/g, ' ')
}

/** Permission bits as octal, the way `ls -l` people read them. */
export function octal(mode: number): string {
  return `0${mode.toString(8).padStart(3, '0')}`
}

/** A `datetime-local` input value as RFC 3339 UTC, or undefined. */
export function localInputToIso(value: string): string | undefined {
  if (!value) return undefined
  const d = new Date(value)
  return Number.isNaN(d.getTime()) ? undefined : d.toISOString()
}

/** `request_log.retain` as sent: a humantime string is shown as is, seconds are converted. */
export function formatRetain(retain: string): string {
  if (!/^\d+$/.test(retain)) return retain
  const secs = Number(retain)
  if (secs % 86_400 === 0) return `${secs / 86_400}d`
  if (secs % 3600 === 0) return `${secs / 3600}h`
  if (secs % 60 === 0) return `${secs / 60}m`
  return `${secs}s`
}
