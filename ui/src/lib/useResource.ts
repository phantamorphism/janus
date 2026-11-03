import { useCallback, useEffect, useRef, useState } from 'react'

export interface Resource<T> {
  /** The latest data; kept while a reload is in flight. */
  data: T | undefined
  /** The error of the current load, if it failed. */
  error: unknown
  loading: boolean
  reload: () => void
}

interface Settled<T> {
  key: string
  data?: T
  error?: unknown
  failed: boolean
}

/**
 * Load `load` whenever `key` changes (or `reload` is called), aborting the
 * previous load. A failed reload keeps the last good data visible beside
 * the error; a load for a different `key` does not show another key's data.
 */
export function useResource<T>(load: (signal: AbortSignal) => Promise<T>, key: string): Resource<T> {
  const loadRef = useRef(load)
  useEffect(() => {
    loadRef.current = load
  })
  const [tick, setTick] = useState(0)
  const requestKey = `${key}#${tick}`
  const [settled, setSettled] = useState<Settled<T> | null>(null)
  const [lastData, setLastData] = useState<{ key: string; data: T } | null>(null)

  useEffect(() => {
    const ctl = new AbortController()
    loadRef.current(ctl.signal).then(
      (data) => {
        if (ctl.signal.aborted) return
        setSettled({ key: requestKey, data, failed: false })
        setLastData({ key, data })
      },
      (error: unknown) => {
        if (ctl.signal.aborted) return
        setSettled({ key: requestKey, error, failed: true })
      },
    )
    return () => ctl.abort()
  }, [key, requestKey])

  const reload = useCallback(() => setTick((t) => t + 1), [])
  const current = settled?.key === requestKey ? settled : null
  return {
    data: current && !current.failed ? current.data : lastData?.key === key ? lastData.data : undefined,
    error: current?.failed ? current.error : undefined,
    loading: current === null,
    reload,
  }
}
