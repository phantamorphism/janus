import { useEffect, useRef, useState, type FormEvent } from 'react'
import { Pause, Play, Radio } from 'lucide-react'
import { errorMessage } from '@/api/client'
import { useApi } from '@/api/context'
import { runTail, type TailState } from '@/api/sse'
import {
  DEFAULT_LIMIT,
  MAX_LIMIT,
  OUTCOMES,
  type OutcomeName,
  type RequestQuery,
  type RequestRecord,
} from '@/api/types'
import { Badge, StatusBadge } from '@/components/Badge'
import { PageHeader } from '@/components/PageHeader'
import { Passage } from '@/components/Passage'
import { Empty, ErrorNotice, Loading } from '@/components/States'
import { Time } from '@/components/Time'
import { formatDuration, localInputToIso, words } from '@/lib/format'
import { matchesFilter, outcomeQualifier } from '@/lib/outcome'
import { outcomeTone } from '@/lib/tone'
import { useUpstreamNames } from '@/lib/upstream'
import { useResource } from '@/lib/useResource'
import { RequestDrawer } from './RequestDrawer'

interface Draft {
  identity: string
  upstream: string
  outcome: OutcomeName | ''
  credential: string
  since: string
  until: string
  limit: number
}

const emptyDraft: Draft = {
  identity: '',
  upstream: '',
  outcome: '',
  credential: '',
  since: '',
  until: '',
  limit: DEFAULT_LIMIT,
}

function toQuery(d: Draft): RequestQuery {
  return {
    identity: d.identity.trim() || undefined,
    upstream: d.upstream || undefined,
    outcome: d.outcome || undefined,
    credential: d.credential.trim() || undefined,
    since: localInputToIso(d.since),
    until: localInputToIso(d.until),
    limit: d.limit,
  }
}

/**
 * How far before a drop the gap query reaches: a record's `received_at` is
 * when the request arrived, but it is committed only after the response
 * finished (the same lookback as `janus log tail`).
 */
const GAP_LOOKBACK_MS = 300_000

/** Live-tail records are kept newest first, at most this many. */
const LIVE_CAP = 500

function mergeUnique(...lists: RequestRecord[][]): RequestRecord[] {
  const seen = new Set<string>()
  const out: RequestRecord[] = []
  for (const list of lists) {
    for (const r of list) {
      if (seen.has(r.request_id)) continue
      seen.add(r.request_id)
      out.push(r)
    }
  }
  return out
}

function tailText(s: TailState | null, paused: boolean, buffered: number): string {
  if (!s) return ''
  if (paused && s.phase === 'open') return buffered ? `Paused, ${buffered} new held back` : 'Paused'
  switch (s.phase) {
    case 'connecting':
      return 'Connecting to the live tail…'
    case 'open':
      return 'Live: new requests appear as they complete'
    case 'waiting':
      return `Connection lost (${s.reason}). Reconnecting in ${Math.ceil(s.retryInMs / 1000)} s.`
    case 'stopped':
      return `Live tail stopped: ${s.reason}`
  }
}

export function Requests() {
  const api = useApi()
  const [draft, setDraft] = useState<Draft>(emptyDraft)
  const upstreams = useUpstreamNames(false)
  const [query, setQuery] = useState<RequestQuery>(() => toQuery(emptyDraft))
  const key = JSON.stringify(query)
  const first = useResource((signal) => api.requests(query, signal), `requests:${key}`)

  const [older, setOlder] = useState<{ key: string; records: RequestRecord[]; next: string | null } | null>(null)
  const [olderBusy, setOlderBusy] = useState(false)
  const [olderError, setOlderError] = useState<unknown>(undefined)

  const [liveOn, setLiveOn] = useState(false)
  const [paused, setPaused] = useState(false)
  const [tail, setTail] = useState<TailState | null>(null)
  const [live, setLive] = useState<{ key: string; records: RequestRecord[] }>({ key, records: [] })
  const [held, setHeld] = useState<{ key: string; records: RequestRecord[] }>({ key, records: [] })
  const [fresh, setFresh] = useState<Set<string>>(new Set())
  const [gapError, setGapError] = useState<string | null>(null)
  const [logDisabled, setLogDisabled] = useState(false)
  const [selected, setSelected] = useState<RequestRecord | null>(null)

  const pausedRef = useRef(paused)
  const queryRef = useRef(query)
  useEffect(() => {
    pausedRef.current = paused
    queryRef.current = query
  })

  const streamUrl = api.streamUrl(query)
  useEffect(() => {
    if (!liveOn) return
    const ctl = new AbortController()
    const startedAt = Date.now()
    // When the stream was last known good before a drop or `lagged`; null
    // while nothing is missing. The gap is closed from here.
    let gapFrom: number | null = null

    function ingest(rec: RequestRecord) {
      if (!matchesFilter(queryRef.current, rec)) return
      // Keyed by the filters in force now: credential/since/until change
      // without reconnecting, and records must land under the current key.
      const streamKey = JSON.stringify(queryRef.current)
      const add = (prev: { key: string; records: RequestRecord[] }) => ({
        key: streamKey,
        records: mergeUnique([rec], prev.key === streamKey ? prev.records : []).slice(0, LIVE_CAP),
      })
      if (pausedRef.current) {
        setHeld(add)
      } else {
        setLive(add)
        setFresh((f) => new Set(f).add(rec.request_id))
      }
    }

    // `GET /v1/requests?since=` for what was committed while the stream was
    // down or behind. Records already shown are dropped by id when merged.
    async function closeGap() {
      if (gapFrom === null) return
      const since = new Date(Math.max(gapFrom - GAP_LOOKBACK_MS, startedAt)).toISOString()
      gapFrom = null
      const base = { ...queryRef.current, since, limit: MAX_LIMIT, before: undefined }
      try {
        const found: RequestRecord[] = []
        let before: string | undefined
        for (;;) {
          const page = await api.requests({ ...base, before }, ctl.signal)
          found.push(...page.requests)
          if (!page.next) break
          before = page.next
        }
        // Pages are newest first; ingest oldest first so the newest ends on top.
        for (const rec of found.reverse()) ingest(rec)
      } catch (e) {
        if (ctl.signal.aborted) return
        setGapError(errorMessage(e))
        return
      }
      setGapError(null)
    }

    runTail({
      url: streamUrl,
      headers: api.headers(),
      signal: ctl.signal,
      onUnauthorized: () => api.unauthorized(),
      onState: (state) => {
        setTail(state)
        if (state.phase === 'waiting') gapFrom ??= Date.now()
      },
      onEvent: (ev) => {
        switch (ev.event) {
          case 'ping':
            // The subscription is in place: nothing committed from here on
            // is missed, so what came before can be queried safely.
            void closeGap()
            break
          case 'lagged':
            gapFrom ??= Date.now()
            void closeGap()
            break
          case 'disabled':
            setLogDisabled(true)
            setLiveOn(false)
            setTail(null)
            ctl.abort()
            break
          case 'request': {
            let rec: RequestRecord
            try {
              rec = JSON.parse(ev.data) as RequestRecord
            } catch {
              return
            }
            if (!rec || typeof rec.request_id !== 'string') return
            ingest(rec)
            break
          }
          default:
            break
        }
      },
    })
    return () => ctl.abort()
  }, [liveOn, streamUrl, api])

  function apply(e: FormEvent) {
    e.preventDefault()
    setQuery(toQuery(draft))
  }

  function reset() {
    setDraft(emptyDraft)
    setQuery(toQuery(emptyDraft))
  }

  function toggleLive() {
    if (liveOn) {
      setLiveOn(false)
      setPaused(false)
      setTail(null)
      setHeld({ key, records: [] })
    } else {
      setLiveOn(true)
    }
  }

  function resume() {
    const heldNow = held.key === key ? held.records : []
    setLive((prev) => ({
      key,
      records: mergeUnique(heldNow, prev.key === key ? prev.records : []).slice(0, LIVE_CAP),
    }))
    setFresh(new Set(heldNow.map((r) => r.request_id)))
    setHeld({ key, records: [] })
    setPaused(false)
  }

  const olderHere = older?.key === key ? older : null
  const cursor = olderHere ? olderHere.next : (first.data?.next ?? null)

  async function loadOlder() {
    if (!cursor) return
    setOlderBusy(true)
    setOlderError(undefined)
    try {
      const page = await api.requests({ ...query, before: cursor })
      setOlder({
        key,
        records: [...(olderHere?.records ?? []), ...page.requests],
        next: page.next,
      })
    } catch (e) {
      setOlderError(e)
    } finally {
      setOlderBusy(false)
    }
  }

  const liveRecords = live.key === key ? live.records : []
  const heldCount = held.key === key ? held.records.length : 0
  const records = first.data
    ? mergeUnique(liveRecords, first.data.requests, olderHere?.records ?? [])
    : liveRecords
  const disabled = first.data?.disabled === true || logDisabled

  return (
    <>
      <PageHeader
        title="Requests"
        lede="Every request that reached the data plane, newest first: who made it, where it went, and how it ended. Metadata only; Janus never records bodies."
        actions={
          <>
            <button
              type="button"
              className={`btn ${liveOn ? 'btn-primary' : ''}`}
              aria-pressed={liveOn}
              onClick={toggleLive}
              disabled={disabled}
            >
              <Radio size={14} aria-hidden="true" /> Live tail
            </button>
            {liveOn && (
              <button
                type="button"
                className="btn"
                onClick={paused ? resume : () => setPaused(true)}
              >
                {paused ? (
                  <>
                    <Play size={14} aria-hidden="true" /> Resume{heldCount ? ` (${heldCount} new)` : ''}
                  </>
                ) : (
                  <>
                    <Pause size={14} aria-hidden="true" /> Pause
                  </>
                )}
              </button>
            )}
          </>
        }
      />

      <form onSubmit={apply} aria-label="Filter requests" className="mb-4 grid grid-cols-2 gap-3 md:grid-cols-4 xl:grid-cols-8">
        <label className="col-span-2 flex flex-col gap-1 text-sm md:col-span-1 xl:col-span-2">
          <span className="text-muted">Identity</span>
          <input
            className="field mono"
            value={draft.identity}
            onChange={(e) => setDraft({ ...draft, identity: e.target.value })}
            spellCheck={false}
            autoComplete="off"
          />
        </label>
        <label className="flex flex-col gap-1 text-sm">
          <span className="text-muted">Upstream</span>
          <select
            className="field"
            value={draft.upstream}
            onChange={(e) => setDraft({ ...draft, upstream: e.target.value })}
          >
            <option value="">Any</option>
            {upstreams.map((u) => (
              <option key={u} value={u}>
                {u}
              </option>
            ))}
          </select>
        </label>
        <label className="flex flex-col gap-1 text-sm">
          <span className="text-muted">Outcome</span>
          <select
            className="field"
            value={draft.outcome}
            onChange={(e) => setDraft({ ...draft, outcome: e.target.value as OutcomeName | '' })}
          >
            <option value="">Any</option>
            {OUTCOMES.map((o) => (
              <option key={o} value={o}>
                {words(o)}
              </option>
            ))}
          </select>
        </label>
        <label className="flex flex-col gap-1 text-sm">
          <span className="text-muted">Credential</span>
          <input
            className="field mono"
            value={draft.credential}
            onChange={(e) => setDraft({ ...draft, credential: e.target.value })}
            spellCheck={false}
            autoComplete="off"
          />
        </label>
        <label className="flex flex-col gap-1 text-sm">
          <span className="text-muted">Since</span>
          <input
            type="datetime-local"
            step={1}
            className="field"
            value={draft.since}
            onChange={(e) => setDraft({ ...draft, since: e.target.value })}
          />
        </label>
        <label className="flex flex-col gap-1 text-sm">
          <span className="text-muted">Until</span>
          <input
            type="datetime-local"
            step={1}
            className="field"
            value={draft.until}
            onChange={(e) => setDraft({ ...draft, until: e.target.value })}
          />
        </label>
        <label className="flex flex-col gap-1 text-sm">
          <span className="text-muted">Page size</span>
          <select
            className="field"
            value={draft.limit}
            onChange={(e) => setDraft({ ...draft, limit: Number(e.target.value) })}
          >
            {[50, 100, 250, 1000].map((n) => (
              <option key={n} value={n}>
                {n}
              </option>
            ))}
          </select>
        </label>
        <div className="col-span-2 flex items-end gap-2 md:col-span-4 xl:col-span-8">
          <button type="submit" className="btn btn-primary">
            Apply filters
          </button>
          <button type="button" className="btn btn-quiet" onClick={reset}>
            Clear
          </button>
        </div>
      </form>

      <div aria-live="polite" className="mb-2 min-h-5 text-sm text-muted">
        {liveOn && <span role="status">{tailText(tail, paused, heldCount)}</span>}
        {liveOn && gapError && (
          <span role="alert" className="ml-2 text-bad">
            Could not fill the gap after the reconnect: {gapError}
          </span>
        )}
      </div>

      {first.error !== undefined && <ErrorNotice error={first.error} what="requests" onRetry={first.reload} />}
      {disabled ? (
        <Empty title="The request log is off">
          This daemon runs with <code className="mono">request_log.enabled = false</code>, so it records no
          requests. Turn it on in the daemon config to see traffic here.
        </Empty>
      ) : first.data === undefined && liveRecords.length === 0 ? (
        first.loading && <Loading what="requests" />
      ) : records.length === 0 ? (
        <Empty title="No requests match">
          {liveOn ? 'Waiting for traffic. New requests appear here as they complete.' : 'Change the filters, or turn on the live tail and send some traffic.'}
        </Empty>
      ) : (
        <>
          <div className="overflow-x-auto rounded-md border border-line bg-surface">
            <table className="data-table" aria-label="Request log">
              <thead>
                <tr>
                  <th scope="col">Received</th>
                  <th scope="col">Caller and upstream</th>
                  <th scope="col">Outcome</th>
                  <th scope="col">Request</th>
                  <th scope="col">Credential</th>
                  <th scope="col" className="text-right">
                    Status
                  </th>
                  <th scope="col" className="text-right">
                    Duration
                  </th>
                </tr>
              </thead>
              <tbody>
                {records.map((r) => {
                  const q = outcomeQualifier(r)
                  return (
                    <tr
                      key={r.request_id}
                      data-request-id={r.request_id}
                      className={`cursor-pointer hover:bg-sunken ${fresh.has(r.request_id) ? 'arrive' : ''}`}
                      onClick={() => setSelected(r)}
                    >
                      <td>
                        <Time value={r.received_at} stacked />
                      </td>
                      <td className="max-w-72">
                        <Passage record={r} />
                      </td>
                      <td>
                        <Badge tone={outcomeTone(r.outcome)}>{words(r.outcome)}</Badge>
                        {q && <div className="mt-1 max-w-56 truncate text-xs text-muted">{q}</div>}
                      </td>
                      <td className="max-w-80">
                        <button
                          type="button"
                          className="mono block max-w-full truncate text-left underline decoration-line-strong underline-offset-2 hover:decoration-ink"
                          onClick={(e) => {
                            e.stopPropagation()
                            setSelected(r)
                          }}
                          aria-label={`Show details of ${r.method} ${r.path} (${r.request_id})`}
                        >
                          {r.method} {r.path}
                        </button>
                      </td>
                      <td className="mono">
                        {r.credential_ref ?? <span className="text-muted">—</span>}
                        {r.attempts.length > 0 && (
                          <div className="font-sans text-xs text-muted">
                            after {r.attempts.map((x) => x.credential_ref).join(', ')}
                          </div>
                        )}
                      </td>
                      <td className="text-right">{r.status === null ? <span className="text-muted">—</span> : <StatusBadge code={r.status} />}</td>
                      <td className="text-right whitespace-nowrap tabular-nums">{formatDuration(r.duration_ms)}</td>
                    </tr>
                  )
                })}
              </tbody>
            </table>
          </div>
          <div className="mt-4 flex items-center gap-3">
            {cursor ? (
              <button type="button" className="btn" onClick={loadOlder} disabled={olderBusy}>
                {olderBusy ? 'Loading…' : 'Load older requests'}
              </button>
            ) : (
              first.data && <p className="text-sm text-muted">That is everything matching these filters.</p>
            )}
            {olderError !== undefined && (
              <p role="alert" className="text-sm text-bad">
                Could not load older requests: {errorMessage(olderError)}
              </p>
            )}
          </div>
        </>
      )}
      {selected && <RequestDrawer initial={selected} onClose={() => setSelected(null)} />}
    </>
  )
}
