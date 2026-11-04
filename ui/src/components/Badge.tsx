import type { ReactNode } from 'react'
import { statusTone, type Tone } from '@/lib/tone'

const toneClass: Record<Tone, string> = {
  ok: 'bg-ok-soft text-ok',
  warn: 'bg-warn-soft text-warn',
  bad: 'bg-bad-soft text-bad',
  idle: 'bg-idle-soft text-idle',
}

const dotClass: Record<Tone, string> = {
  ok: 'bg-ok',
  warn: 'bg-warn',
  bad: 'bg-bad',
  idle: 'bg-idle',
}

/** A status word with a tone. The word always carries the meaning; colour only repeats it. */
export function Badge({ tone, children }: { tone: Tone; children: ReactNode }) {
  return (
    <span
      className={`inline-flex items-center gap-1.5 rounded-full px-2 py-0.5 text-xs font-medium whitespace-nowrap ${toneClass[tone]}`}
    >
      <span aria-hidden="true" className={`h-1.5 w-1.5 rounded-full ${dotClass[tone]}`} />
      {children}
    </span>
  )
}

/** An upstream HTTP status code, coloured by its class (2xx, 3xx, 4xx, 5xx). */
export function StatusBadge({ code }: { code: number }) {
  return (
    <span title={`Upstream replied ${code}`}>
      <Badge tone={statusTone(code)}>{code}</Badge>
    </span>
  )
}
