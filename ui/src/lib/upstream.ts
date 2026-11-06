import { useApi } from '@/api/context'
import { UPSTREAMS, type CredentialFilter, type RequestRecord, type Strategy } from '@/api/types'
import { useResource } from './useResource'

/**
 * An instance as it is read: its name, with the implementation beside it when
 * the two differ (`claude (anthropic)`), the same rendering as `janus log`.
 */
export function instanceLabel(name: string, implementation: string | null | undefined): string {
  return implementation && implementation !== name ? `${name} (${implementation})` : name
}

export function recordUpstream(r: Pick<RequestRecord, 'upstream' | 'implementation'>): string | null {
  return r.upstream === null ? null : instanceLabel(r.upstream, r.implementation)
}

const strategyLabel: Record<Strategy, string> = {
  ordered: 'ordered',
  round_robin: 'round robin',
  least_recently_used: 'least recently used',
  sticky: 'sticky by identity',
}

export function selectionText(strategy: Strategy, failover: boolean): string {
  return `${strategyLabel[strategy] ?? strategy}${failover ? ', fails over' : ''}`
}

/** The filter in words; null when it admits every credential. */
export function filterText(f: CredentialFilter | null): string | null {
  if (f === null) return null
  const parts: string[] = []
  if (f.kind) parts.push(f.kind === 'oauth' ? 'OAuth only' : 'API keys only')
  if (f.refs) parts.push(`refs ${f.refs.join(', ')}`)
  if (f.tags) parts.push(`tagged ${f.tags.join(' or ')}`)
  return parts.join('; ')
}

/**
 * The `[[upstream]]` instance names from `/v1/status`, in config order, for an
 * upstream filter. With `implementations`, implementation names no instance
 * has are appended (the credentials endpoint accepts them). Until status loads,
 * or if it fails, the implementation names stand in.
 */
export function useUpstreamNames(implementations: boolean): string[] {
  const api = useApi()
  const status = useResource((signal) => api.status(signal), 'status')
  const names = status.data?.upstreams.map((u) => u.name)
  if (!names) return [...UPSTREAMS]
  return implementations ? [...names, ...UPSTREAMS.filter((u) => !names.includes(u))] : names
}
