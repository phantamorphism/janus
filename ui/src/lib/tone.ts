import type { CredentialStatus, OutcomeName, PluginHealth } from '@/api/types'

/** The four states anything in the console can be in, visually. */
export type Tone = 'ok' | 'warn' | 'bad' | 'idle'

export function credentialTone(s: CredentialStatus): Tone {
  switch (s) {
    case 'active':
      return 'ok'
    case 'rate_limited':
      return 'warn'
    case 'invalid':
      return 'bad'
    case 'disabled':
      return 'idle'
  }
}

export function healthTone(h: PluginHealth): Tone {
  if (h === 'ok') return 'ok'
  if (h === 'unknown') return 'idle'
  return 'bad'
}

export function outcomeTone(o: OutcomeName): Tone {
  switch (o) {
    case 'forwarded':
      return 'ok'
    case 'rejected_identity':
    case 'denied':
      return 'bad'
    default:
      return 'warn'
  }
}
