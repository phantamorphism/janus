import type { RequestRecord } from '@/api/types'
import { outcomeTone, type Tone } from '@/lib/tone'

const stroke: Record<Tone, string> = {
  ok: 'text-ok',
  warn: 'text-warn',
  bad: 'text-bad',
  idle: 'text-idle',
}

/**
 * One request as a passage through the gate: the caller on one side, the
 * upstream on the other. A forwarded request's gate stands open; any other
 * outcome shows it shut, in the outcome's tone.
 */
export function Passage({ record }: { record: RequestRecord }) {
  const tone = outcomeTone(record.outcome)
  const open = record.outcome === 'forwarded'
  const caller = record.caller_identity
  return (
    <span className="inline-flex max-w-full items-center gap-2">
      <span className="mono min-w-0 truncate text-right">
        {caller ? (
          caller
        ) : record.claimed_identity ? (
          <span className="text-muted italic" title="Claimed by the material, not verified">
            {record.claimed_identity}?
          </span>
        ) : (
          <span className="text-muted">{record.binding === 'none' ? 'open route' : 'unidentified'}</span>
        )}
      </span>
      <svg
        viewBox="0 0 24 24"
        className={`h-4 w-4 shrink-0 ${stroke[tone]}`}
        fill="none"
        stroke="currentColor"
        strokeWidth={2}
        strokeLinecap="round"
        aria-hidden="true"
      >
        <path d="M5 21V10a7 7 0 0 1 14 0v11" />
        {open ? <path d="M3 21h18" /> : <path d="M7.5 21V10.5h9V21M3 21h18" />}
      </svg>
      <span className="min-w-0 truncate">{record.upstream ?? <span className="text-muted">no upstream</span>}</span>
    </span>
  )
}
