import { useCallback, useEffect, useMemo, useState, type MouseEvent } from 'react'
import { LogOut } from 'lucide-react'
import { ApiClient } from '@/api/client'
import { ApiContext } from '@/api/context'
import { TokenGate } from '@/auth/TokenGate'
import { clearToken, loadToken, saveToken } from '@/auth/token'
import { JanusMark } from '@/components/JanusMark'
import { API_BASE } from '@/config'
import { VIEWS, VIEW_TITLES, pathForView, viewFromPath, type View } from '@/lib/route'
import { Credentials } from '@/views/Credentials'
import { Identities } from '@/views/Identities'
import { Overview } from '@/views/Overview'
import { Plugin } from '@/views/Plugin'
import { Requests } from '@/views/Requests'

const viewComponents: Record<View, () => React.JSX.Element> = {
  overview: Overview,
  credentials: Credentials,
  identities: Identities,
  requests: Requests,
  plugin: Plugin,
}

export default function App() {
  const [token, setToken] = useState<string | null>(() => loadToken())
  const [reason, setReason] = useState<string | null>(null)
  const [view, setView] = useState<View>(() => viewFromPath(window.location.pathname))

  useEffect(() => {
    const onPop = () => setView(viewFromPath(window.location.pathname))
    window.addEventListener('popstate', onPop)
    return () => window.removeEventListener('popstate', onPop)
  }, [])

  useEffect(() => {
    document.title = `${VIEW_TITLES[view]} – Janus`
  }, [view])

  const signOut = useCallback((why: string | null) => {
    clearToken()
    setReason(why)
    setToken(null)
  }, [])

  const api = useMemo(
    () =>
      token
        ? new ApiClient({
            baseUrl: API_BASE,
            token,
            onUnauthorized: () => signOut('The daemon no longer accepts the saved token. Enter it again.'),
          })
        : null,
    [token, signOut],
  )

  function navigate(e: MouseEvent<HTMLAnchorElement>, to: View) {
    if (e.metaKey || e.ctrlKey || e.shiftKey || e.altKey || e.button !== 0) return
    e.preventDefault()
    if (to !== view) window.history.pushState(null, '', pathForView(to))
    setView(to)
  }

  if (!api) {
    return (
      <TokenGate
        apiBase={API_BASE}
        reason={reason}
        onAccept={(t) => {
          saveToken(t)
          setReason(null)
          setToken(t)
        }}
      />
    )
  }

  const Current = viewComponents[view]
  return (
    <ApiContext.Provider value={api}>
      <div className="min-h-screen md:grid md:grid-cols-[13.5rem_1fr]">
        <a
          href="#main"
          className="sr-only focus:not-sr-only focus:absolute focus:top-2 focus:left-2 focus:z-50 focus:bg-surface focus:px-3 focus:py-2"
        >
          Skip to content
        </a>
        <aside className="border-b border-line bg-surface md:sticky md:top-0 md:h-screen md:border-r md:border-b-0">
          <div className="flex items-center gap-2.5 px-5 pt-5 pb-4">
            <JanusMark className="h-7 w-7 text-brass" />
            <span className="font-serif text-2xl leading-none">Janus</span>
          </div>
          <nav aria-label="Views" className="px-3 pb-3 md:pb-0">
            <ul className="flex gap-1 overflow-x-auto md:flex-col">
              {VIEWS.map((v) => (
                <li key={v}>
                  <a
                    href={pathForView(v)}
                    onClick={(e) => navigate(e, v)}
                    aria-current={v === view ? 'page' : undefined}
                    className={`block rounded-md border-l-2 px-3 py-1.5 text-sm whitespace-nowrap ${
                      v === view
                        ? 'border-brass bg-brass-soft font-medium text-ink'
                        : 'border-transparent text-ink-soft hover:bg-sunken'
                    }`}
                  >
                    {VIEW_TITLES[v]}
                  </a>
                </li>
              ))}
            </ul>
          </nav>
          <div className="px-5 pb-3 md:pt-6 md:pb-0">
            <button type="button" className="btn btn-quiet -ml-2 text-muted" onClick={() => signOut(null)}>
              <LogOut size={14} aria-hidden="true" /> Forget token
            </button>
          </div>
        </aside>
        <main id="main" className="min-w-0 px-5 py-8 md:px-10">
          <div className="mx-auto max-w-6xl">
            <Current />
          </div>
        </main>
      </div>
    </ApiContext.Provider>
  )
}
